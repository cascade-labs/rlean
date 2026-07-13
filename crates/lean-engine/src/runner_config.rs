use crate::artifacts::RunArtifactSink;
use crate::data_feed::DataFeedOptions;
use lean_algorithm::charting::ChartCollection;
use lean_algorithm::qc_algorithm::BrokerageName;
use lean_alpha::AlphaAnalytics;
use lean_brokerages::Brokerage;
use lean_data_providers::{ICustomDataSource, IHistoryProvider};
use lean_live::DataQueueHandlerManager;
use lean_orders::{Order, OrderEvent};
use lean_statistics::PortfolioStatistics;
use lean_storage::IcebergStore;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BacktestProgress {
    pub current_date: chrono::NaiveDate,
    pub start_date: chrono::NaiveDate,
    pub end_date: chrono::NaiveDate,
    pub trading_days: i64,
    pub portfolio_value: f64,
}

pub struct BacktestRunConfig {
    pub data_root: PathBuf,
    pub data_store: Arc<IcebergStore>,
    pub _compression_level: i32,
    /// Raw stacked provider for DataType-specific requests (e.g. FactorFile).
    /// Providers that don't support a DataType return NotImplemented and the
    /// next provider in the stack is tried.
    pub history_provider: Option<Arc<dyn IHistoryProvider>>,
    /// Override the strategy's set_start_date (YYYY-MM-DD).
    pub start_date_override: Option<chrono::NaiveDate>,
    /// Override the strategy's set_end_date (YYYY-MM-DD).
    pub end_date_override: Option<chrono::NaiveDate>,
    /// Algorithm parameters available through QCAlgorithm.get_parameter().
    pub parameters: HashMap<String, String>,
    /// Custom data source plugins loaded from `~/.rlean/plugins/` or set explicitly.
    pub custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
    /// Subscription feed caching/prefetch behavior.
    pub data_feed_options: DataFeedOptions,
    /// Optional backtest output directory. When set, progress/order/trade sidecar
    /// files are written while the backtest is still running.
    pub output_dir: Option<PathBuf>,
    /// Optional artifact sink. When set, streaming files are written into the
    /// sink's working dir and mirrored to S3 per the sink's mode. Takes
    /// precedence over `output_dir` for the streaming writer's location.
    pub artifact_sink: Option<Arc<RunArtifactSink>>,
    pub progress: Option<Arc<dyn Fn(BacktestProgress) + Send + Sync>>,
}

pub struct LiveRunConfig {
    pub data_root: PathBuf,
    pub data_store: Arc<IcebergStore>,
    pub history_provider: Option<Arc<dyn IHistoryProvider>>,
    pub parameters: HashMap<String, String>,
    pub custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
    pub live_data_queue: DataQueueHandlerManager,
    /// Optional real brokerage adapter. With `paper_trading=true`, the
    /// brokerage is connected and synced while orders are locally acknowledged
    /// and filled by the paper fill model. With `paper_trading=false`, new
    /// orders are submitted to the brokerage before fill events are processed.
    pub brokerage: Option<Box<dyn Brokerage>>,
    /// Optional brokerage model selected by the live CLI. This is applied
    /// before Initialize(), so user code can still override it explicitly.
    pub brokerage_model: Option<BrokerageName>,
    pub paper_trading: bool,
    /// Stops after this many emitted slices. Intended for integration tests and
    /// smoke runs; `None` runs until every live subscription closes.
    pub max_slices: Option<usize>,
    /// Stops the live run after this wall-clock duration. Intended for paper
    /// deployment soaks and integration tests.
    pub max_runtime: Option<Duration>,
    /// Optional live deployment directory. When set, live portfolio/order/log
    /// sidecars are written while the run is still active.
    pub output_dir: Option<PathBuf>,
    /// Optional artifact sink. When set, snapshots are written into the sink's
    /// working dir and mirrored to S3 asynchronously per the sink's mode.
    pub artifact_sink: Option<Arc<RunArtifactSink>>,
}

pub struct LiveRunResult {
    pub slices_processed: usize,
    pub final_value: f64,
    pub order_events: Vec<OrderEvent>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub stopped_at: chrono::DateTime<chrono::Utc>,
}

pub struct BacktestRunResult {
    pub trading_days: i64,
    pub final_value: f64,
    pub total_return: f64,
    pub starting_cash: f64,
    pub total_fees: f64,
    pub total_funding: f64,
    pub start_date: chrono::NaiveDate,
    pub end_date: chrono::NaiveDate,
    /// Daily portfolio values (one per trading day, in order).
    pub equity_curve: Vec<f64>,
    /// ISO date strings matching equity_curve.
    pub daily_dates: Vec<String>,
    /// Daily benchmark prices, in order.
    pub benchmark_curve: Vec<f64>,
    /// ISO date strings matching benchmark_curve.
    pub benchmark_dates: Vec<String>,
    /// Full statistics computed at the end of the backtest.
    pub statistics: PortfolioStatistics,
    /// Custom strategy charts plotted via self.plot().
    pub charts: ChartCollection,
    /// All order fill events from the backtest run.
    pub order_events: Vec<OrderEvent>,
    /// Final order states from the backtest run.
    pub orders: Vec<Order>,
    /// Symbols/dates for which data was found in the Parquet store.
    pub succeeded_data_requests: Vec<String>,
    /// Symbols/dates for which no data was found.
    pub failed_data_requests: Vec<String>,
    /// Unix epoch seconds at backtest start (used as backtest ID).
    pub backtest_id: i64,
    /// The ticker used as the benchmark (e.g. "SPY").
    pub benchmark_symbol: String,
    /// Alpha-framework diagnostics: rolling IC, signal correlation, ranking.
    pub alpha_analytics: AlphaAnalytics,
}

impl BacktestRunResult {
    pub fn print_summary(&self) {
        use rust_decimal::prelude::ToPrimitive;
        let s = &self.statistics;
        println!("╔══════════════════════════════════════════════════════╗");
        println!("║                  Backtest Complete                   ║");
        println!("╠══════════════════════════════════════════════════════╣");
        let row = |label: &str, value: &str| {
            println!("║  {:<30} {:>20}  ║", label, value);
        };
        row("Start Date", &self.start_date.to_string());
        row("End Date", &self.end_date.to_string());
        row("Trading Days", &self.trading_days.to_string());
        row("Starting Cash", &format!("${:.2}", self.starting_cash));
        row("Final Value", &format!("${:.2}", self.final_value));
        row("Total Fees", &format!("${:.2}", self.total_fees));
        row("Total Funding", &format!("${:.2}", self.total_funding));
        row(
            "Total Return",
            &format!("{:.2}%", self.total_return * 100.0),
        );
        row(
            "CAGR",
            &format!(
                "{:.2}%",
                s.compounding_annual_return.to_f64().unwrap_or(0.0) * 100.0
            ),
        );
        row(
            "Sharpe Ratio",
            &format!("{:.3}", s.sharpe_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Sortino Ratio",
            &format!("{:.3}", s.sortino_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Probabilistic SR",
            &format!(
                "{:.1}%",
                s.probabilistic_sharpe_ratio.to_f64().unwrap_or(0.0) * 100.0
            ),
        );
        row(
            "Calmar Ratio",
            &format!("{:.3}", s.calmar_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Omega Ratio",
            &format!("{:.3}", s.omega_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Max Drawdown",
            &format!("{:.2}%", s.drawdown.to_f64().unwrap_or(0.0) * 100.0),
        );
        row(
            "Recovery Factor",
            &format!("{:.2}", s.recovery_factor.to_f64().unwrap_or(0.0)),
        );
        row(
            "Annual Std Dev",
            &format!(
                "{:.2}%",
                s.annual_standard_deviation.to_f64().unwrap_or(0.0) * 100.0
            ),
        );
        row(
            "Alpha",
            &format!("{:.2}%", s.alpha.to_f64().unwrap_or(0.0) * 100.0),
        );
        row("Beta", &format!("{:.3}", s.beta.to_f64().unwrap_or(0.0)));
        row(
            "Treynor Ratio",
            &format!("{:.3}", s.treynor_ratio.to_f64().unwrap_or(0.0)),
        );
        println!("╚══════════════════════════════════════════════════════╝");
    }
}
