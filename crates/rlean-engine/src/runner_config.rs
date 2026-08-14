use crate::data_feed::DataFeedOptions;
use crate::live::catalog_state::LiveRestoreState;
use rlean_algorithm::charting::ChartCollection;
use rlean_algorithm::qc_algorithm::BrokerageModel;
use rlean_alpha::{AlphaAnalytics, InsightEvent};
use rlean_brokerages::Brokerage;
use rlean_data_providers::{HistoricalDataProvider, LiveDataProvider};
use rlean_orders::{Order, OrderEvent};
use rlean_statistics::{PortfolioStatistics, Trade};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct BacktestProgress {
    pub current_date: chrono::NaiveDate,
    pub start_date: chrono::NaiveDate,
    pub end_date: chrono::NaiveDate,
    pub trading_days: i64,
    pub starting_cash: f64,
    pub portfolio_value: f64,
}

/// Durable run telemetry shared by backtest and live. Consumed by `RunCatalog`
/// and appended to the same Verglas tables (`rlean.runs`, `run_progress`,
/// `order_events`, `trades`, `insights`, `insight_state`, `checkpoints`).
#[derive(Debug, Clone)]
pub struct BacktestStreamUpdate {
    pub progress: BacktestProgress,
    pub record_daily_progress: bool,
    pub order_events: Vec<OrderEvent>,
    pub trades: Vec<Trade>,
    pub insight_events: Vec<InsightEvent>,
    /// Optional account checkpoint JSON (portfolio + open orders).
    /// Live emits this on account state changes; backtests leave it `None`.
    pub checkpoint_json: Option<String>,
    /// Optional complete framework insight snapshot JSON.
    /// Live persists this independently from brokerage account state.
    pub insight_state_json: Option<String>,
}

impl BacktestStreamUpdate {
    pub fn empty_with_progress(progress: BacktestProgress, record_daily_progress: bool) -> Self {
        Self {
            progress,
            record_daily_progress,
            order_events: Vec::new(),
            trades: Vec::new(),
            insight_events: Vec::new(),
            checkpoint_json: None,
            insight_state_json: None,
        }
    }
}

pub struct BacktestRunConfig {
    pub historical_provider: Arc<dyn HistoricalDataProvider>,
    pub _compression_level: i32,
    /// Override the strategy's set_start_date (YYYY-MM-DD).
    pub start_date_override: Option<chrono::NaiveDate>,
    /// Override the strategy's set_end_date (YYYY-MM-DD).
    pub end_date_override: Option<chrono::NaiveDate>,
    /// Algorithm parameters available through QCAlgorithm.get_parameter().
    pub parameters: HashMap<String, String>,
    /// Subscription feed caching/prefetch behavior.
    pub data_feed_options: DataFeedOptions,
    pub progress: Option<Arc<dyn Fn(BacktestProgress) + Send + Sync>>,
    /// Bounded, lossless stream of durable run updates. The engine awaits
    /// capacity instead of dropping fills when the catalog writer falls behind.
    pub stream_updates: Option<tokio::sync::mpsc::Sender<BacktestStreamUpdate>>,
}

pub struct LiveRunConfig {
    pub historical_provider: Arc<dyn HistoricalDataProvider>,
    pub live_data_provider: Arc<dyn LiveDataProvider>,
    pub parameters: HashMap<String, String>,
    /// Optional authenticated execution brokerage. Market data remains an
    /// independent provider. `None` keeps execution in rlean's paper path.
    pub brokerage: Option<Box<dyn Brokerage>>,
    /// Brokerage model selected by the live deployment. The execution
    /// brokerage and account type are modeled together, matching C# LEAN's
    /// `IBrokerageModel` boundary.
    pub brokerage_model: BrokerageModel,
    pub paper_trading: bool,
    /// Stops after this many emitted slices. Intended for integration tests and
    /// smoke runs; `None` runs until every live subscription closes.
    pub max_slices: Option<usize>,
    /// Stops the live run after this wall-clock duration. Intended for paper
    /// deployment soaks and integration tests.
    pub max_runtime: Option<Duration>,
    /// Bounded stream of durable catalog updates (same shape as backtests).
    pub stream_updates: Option<tokio::sync::mpsc::Sender<BacktestStreamUpdate>>,
    /// Wall-clock start of this deployment; used as the progress window start.
    pub deploy_started_at: chrono::DateTime<chrono::Utc>,
    /// Optional restore payload loaded from the Verglas catalog before start.
    pub restore: Option<LiveRestoreState>,
    /// Deployment id used when tagging durable insight state.
    pub deploy_id: Option<String>,
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
    /// Completed round-trip trades used to compute the final statistics.
    pub trades: Vec<Trade>,
    /// Complete framework insight lifecycle, in engine event order.
    pub insight_events: Vec<InsightEvent>,
    /// Symbols/dates for which the configured provider returned data.
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
