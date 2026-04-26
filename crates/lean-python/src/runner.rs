/// Standalone Python strategy runner.
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Datelike, NaiveDate};
use pyo3::prelude::*;
use pyo3::types::PyType;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json;
use tracing::{info, warn};

use lean_algorithm::algorithm::IAlgorithm;
use lean_core::{
    exchange_hours::ExchangeHours, DateTime, Market, OptionRight, OptionStyle, Resolution,
    SecurityType, Symbol, SymbolOptionsExt, TickType, TimeSpan,
};
use lean_data::{
    CustomDataConfig, CustomDataFormat, CustomDataPoint, CustomDataQuery, CustomDataSubscription,
    CustomDataTransport, Delisting, DelistingType, IHistoricalDataProvider, QuoteBar, Slice,
    SubscriptionDataConfig, SymbolChangedEvent, Tick, TradeBar, TradeBarData,
};
use lean_options::payoff::{get_exercise_quantity, intrinsic_value};
use lean_options::{
    evaluate_contract_with_market_iv, BlackScholesPriceModel, OptionChain, OptionContract,
    OptionContractData,
};
use lean_orders::{
    fee_model::{FeeModel, InteractiveBrokersFeeModel, OrderFeeParameters},
    fill_model::ImmediateFillModel,
    order_event::OrderEvent,
    order_processor::OrderProcessor,
    slippage::NullSlippageModel,
};
use lean_statistics::{PortfolioStatistics, Trade};
use lean_storage::{
    custom_data_history_path, custom_data_path, option_eod_path, DataCache, FactorFileEntry,
    MapFileEntry, OptionEodBar, OptionUniverseRow, ParquetReader, ParquetWriter, PathResolver,
    QueryParams, WriterConfig,
};

use crate::charting::ChartCollection;
use crate::py_adapter::{set_algorithm_time, PyAlgorithmAdapter};
use crate::py_data::SliceProxy;
use crate::py_framework::run_framework_pipeline;
use lean_data_providers::IHistoryProvider as SyncHistoryProvider;

const HIGH_RESOLUTION_PREFETCH_CONCURRENCY: usize = 8;
const SUBSCRIPTION_PREFETCH_CONCURRENCY: usize = 8;
const CUSTOM_DATA_SOURCE_TIMEOUT: Duration = Duration::from_secs(180);

pub struct RunConfig {
    pub data_root: PathBuf,
    pub _compression_level: i32,
    /// If set, missing price data is fetched from this provider before the backtest loop.
    pub historical_provider: Option<Arc<dyn IHistoricalDataProvider>>,
    /// Raw stacked provider for DataType-specific requests (e.g. FactorFile).
    /// Providers that don't support a DataType return NotImplemented: and the
    /// next provider in the stack is tried.
    pub history_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    /// Override the strategy's set_start_date (YYYY-MM-DD).
    pub start_date_override: Option<chrono::NaiveDate>,
    /// Override the strategy's set_end_date (YYYY-MM-DD).
    pub end_date_override: Option<chrono::NaiveDate>,
    /// Custom data source plugins loaded from `~/.rlean/plugins/` or set explicitly.
    /// Keyed by `source_type` name (e.g. `"fred"`, `"cboe_vix"`).
    pub custom_data_sources: Vec<Arc<dyn lean_data_providers::ICustomDataSource>>,
    /// Optional backtest output directory. When set, progress/order/trade sidecar
    /// files are written while the backtest is still running.
    pub output_dir: Option<PathBuf>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            data_root: PathBuf::from("data"),
            _compression_level: 3,
            historical_provider: None,
            history_provider: None,
            start_date_override: None,
            end_date_override: None,
            custom_data_sources: vec![],
            output_dir: None,
        }
    }
}

pub struct BacktestResult {
    pub trading_days: i64,
    pub final_value: f64,
    pub total_return: f64,
    pub starting_cash: f64,
    pub start_date: chrono::NaiveDate,
    pub end_date: chrono::NaiveDate,
    /// Daily portfolio values (one per trading day, in order).
    pub equity_curve: Vec<f64>,
    /// ISO date strings matching equity_curve.
    pub daily_dates: Vec<String>,
    /// Full statistics computed at the end of the backtest.
    pub statistics: PortfolioStatistics,
    /// Custom strategy charts plotted via self.plot().
    pub charts: ChartCollection,
    /// All order fill events from the backtest run.
    pub order_events: Vec<OrderEvent>,
    /// Symbols/dates for which data was found in the Parquet store.
    pub succeeded_data_requests: Vec<String>,
    /// Symbols/dates for which no data was found.
    pub failed_data_requests: Vec<String>,
    /// Unix epoch seconds at backtest start (used as backtest ID).
    pub backtest_id: i64,
    /// The ticker used as the benchmark (e.g. "SPY").
    pub benchmark_symbol: String,
}

struct LiveBacktestWriter {
    dir: PathBuf,
    progress_path: PathBuf,
    order_events_path: PathBuf,
    trades_path: PathBuf,
    heartbeat_path: PathBuf,
    start_date: NaiveDate,
    end_date: NaiveDate,
    started_at: chrono::DateTime<chrono::Utc>,
    last_log_date: Option<NaiveDate>,
    last_heartbeat: Instant,
}

impl LiveBacktestWriter {
    fn new(dir: PathBuf, start_date: NaiveDate, end_date: NaiveDate) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let writer = LiveBacktestWriter {
            progress_path: dir.join("progress.json"),
            order_events_path: dir.join("order-events.jsonl"),
            trades_path: dir.join("trades.jsonl"),
            heartbeat_path: dir.join("heartbeat.log"),
            dir,
            start_date,
            end_date,
            started_at: chrono::Utc::now(),
            last_log_date: None,
            last_heartbeat: Instant::now() - Duration::from_secs(60),
        };
        let _ = std::fs::File::create(&writer.order_events_path);
        let _ = std::fs::File::create(&writer.trades_path);
        let _ = std::fs::File::create(&writer.heartbeat_path);
        writer
    }

    fn progress_fraction(&self, current_date: NaiveDate) -> f64 {
        let total = (self.end_date - self.start_date).num_days().max(1) as f64;
        let done = (current_date - self.start_date).num_days().max(0) as f64;
        (done / total).clamp(0.0, 1.0)
    }

    fn record_progress(
        &mut self,
        current_date: NaiveDate,
        trading_days: i64,
        portfolio_value: Decimal,
        order_events: usize,
        trades: usize,
    ) {
        let progress = self.progress_fraction(current_date);
        let payload = serde_json::json!({
            "status": "running",
            "current_date": current_date.to_string(),
            "start_date": self.start_date.to_string(),
            "end_date": self.end_date.to_string(),
            "progress": progress,
            "progress_percent": (progress * 100.0),
            "trading_days": trading_days,
            "portfolio_value": portfolio_value.to_string(),
            "order_events": order_events,
            "trades": trades,
            "started_at": self.started_at.to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        let tmp = self.progress_path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(&tmp, json);
            let _ = std::fs::rename(&tmp, &self.progress_path);
        }

        if self.last_log_date != Some(current_date) {
            info!(
                "Backtest progress: {} ({:.1}%) trading_days={} portfolio={} orders={} trades={} output={}",
                current_date,
                progress * 100.0,
                trading_days,
                portfolio_value,
                order_events,
                trades,
                self.dir.display()
            );
            self.last_log_date = Some(current_date);
        }

        if self.last_heartbeat.elapsed() >= Duration::from_secs(30) {
            self.append_heartbeat(current_date, progress, trading_days, portfolio_value);
            self.last_heartbeat = Instant::now();
        }
    }

    fn append_order_events(&self, events: &[OrderEvent]) {
        append_json_lines(&self.order_events_path, events);
    }

    fn append_trades(&self, trades: &[Trade]) {
        append_json_lines(&self.trades_path, trades);
    }

    fn append_heartbeat(
        &self,
        current_date: NaiveDate,
        progress: f64,
        trading_days: i64,
        portfolio_value: Decimal,
    ) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.heartbeat_path)
        {
            let _ = writeln!(
                file,
                "{} current_date={} progress={:.3} trading_days={} portfolio={}",
                chrono::Utc::now().to_rfc3339(),
                current_date,
                progress,
                trading_days,
                portfolio_value
            );
        }
    }

    fn mark_completed(
        &self,
        trading_days: i64,
        portfolio_value: Decimal,
        order_events: usize,
        trades: usize,
    ) {
        let payload = serde_json::json!({
            "status": "completed",
            "current_date": self.end_date.to_string(),
            "start_date": self.start_date.to_string(),
            "end_date": self.end_date.to_string(),
            "progress": 1.0,
            "progress_percent": 100.0,
            "trading_days": trading_days,
            "portfolio_value": portfolio_value.to_string(),
            "order_events": order_events,
            "trades": trades,
            "started_at": self.started_at.to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Ok(json) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(&self.progress_path, json);
        }
    }
}

fn append_json_lines<T: serde::Serialize>(path: &Path, values: &[T]) {
    if values.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        for value in values {
            if let Ok(line) = serde_json::to_string(value) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

impl BacktestResult {
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

/// Load a Python strategy file, find the `QcAlgorithm` subclass,
/// instantiate it, and return a `PyAlgorithmAdapter` ready to run.
pub fn load_strategy(py: Python<'_>, strategy_path: &Path) -> Result<PyAlgorithmAdapter> {
    // Add the strategy directory to sys.path.
    let parent = strategy_path.parent().unwrap_or(Path::new("."));
    let sys = py.import("sys").context("failed to import sys")?;
    let path_list = sys.getattr("path").context("no sys.path")?;
    if let Some(site_packages) = rlean_python_site_packages(py)? {
        path_list
            .call_method1("insert", (0, site_packages.to_string_lossy().as_ref()))
            .context("failed to insert rlean Python site-packages to sys.path")?;
    }
    path_list
        .call_method1("insert", (0, parent.to_string_lossy().as_ref()))
        .context("failed to insert to sys.path")?;

    // Read and compile the strategy source.
    let code_str = std::fs::read_to_string(strategy_path)
        .with_context(|| format!("cannot read {}", strategy_path.display()))?;
    let filename_str = strategy_path.to_string_lossy().to_string();

    // pyo3 0.23 requires &CStr
    use std::ffi::CString;
    let code_c = CString::new(code_str.as_str()).context("strategy code contains null byte")?;
    let filename_c = CString::new(filename_str.as_str()).context("filename contains null byte")?;
    let modname_c = CString::new("strategy").unwrap();

    let module = PyModule::from_code(py, &code_c, &filename_c, &modname_c)
        .with_context(|| format!("failed to compile {}", strategy_path.display()))?;

    // Get the QCAlgorithm base class from the AlgorithmImports module.
    // Try AlgorithmImports first (new name), fall back to lean_rust (old name).
    let lean_mod = py.import("AlgorithmImports")
        .or_else(|_| py.import("lean_rust"))
        .context("AlgorithmImports not importable — was append_to_inittab!(lean_python::AlgorithmImports) called before Python::initialize()?")?;
    let base_class = lean_mod
        .getattr("QCAlgorithm")
        .or_else(|_| lean_mod.getattr("QcAlgorithm"))
        .context("QCAlgorithm not found in AlgorithmImports")?;

    // Walk the module namespace to find the first QcAlgorithm subclass.
    let builtins = py.import("builtins")?;
    let issubclass_fn = builtins.getattr("issubclass")?;

    let mut strategy_class: Option<Bound<'_, PyAny>> = None;
    for (_, value) in module.dict() {
        if !value.is_instance_of::<PyType>() {
            continue;
        }
        if value.eq(&base_class).unwrap_or(false) {
            continue;
        }

        let is_sub = issubclass_fn
            .call1((&value, &base_class))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false);

        if is_sub {
            let name = value
                .getattr("__name__")
                .map(|n| n.to_string())
                .unwrap_or_default();
            info!("Found strategy class: {}", name);
            strategy_class = Some(value);
            break;
        }
    }

    let cls = strategy_class.ok_or_else(|| {
        anyhow::anyhow!(
            "No QcAlgorithm subclass found in {}",
            strategy_path.display()
        )
    })?;

    let instance = cls
        .call0()
        .context("failed to instantiate strategy class")?;
    let instance_py = instance.unbind();

    PyAlgorithmAdapter::from_instance(py, instance_py)
        .context("strategy class must inherit from AlgorithmImports.QCAlgorithm")
}

fn rlean_python_site_packages(py: Python<'_>) -> Result<Option<PathBuf>> {
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return Ok(None),
    };
    let sys = py.import("sys").context("failed to import sys")?;
    let version_info = sys
        .getattr("version_info")
        .context("failed to read sys.version_info")?;
    let major: u8 = version_info.getattr("major")?.extract()?;
    let minor: u8 = version_info.getattr("minor")?.extract()?;
    let site_packages = PathBuf::from(home)
        .join(".rlean")
        .join("python")
        .join(format!("cp{major}{minor}"))
        .join("site-packages");
    Ok(site_packages.exists().then_some(site_packages))
}

/// Run the full backtest loop for a Python strategy.
///
/// Must be called from within an existing tokio runtime (e.g. via `.await`).
/// Do NOT decorate call-sites with `#[tokio::main]` — the caller's runtime
/// is reused so that tokio primitives (Mutex, Semaphore, reqwest) in the
/// historical provider work correctly across the same runtime context.
pub async fn run_strategy(strategy_path: &Path, config: RunConfig) -> Result<BacktestResult> {
    let mut adapter = Python::attach(|py| load_strategy(py, strategy_path))?;

    // ── initialize ──────────────────────────────────────────────────────────
    adapter
        .initialize()
        .context("strategy initialize() failed")?;

    let start_date = config
        .start_date_override
        .unwrap_or_else(|| adapter.start_date().date_utc());
    let end_date = config
        .end_date_override
        .unwrap_or_else(|| adapter.end_date().date_utc());

    let starting_cash = {
        use rust_decimal::prelude::ToPrimitive;
        adapter
            .inner
            .lock()
            .unwrap()
            .portfolio_value()
            .to_f64()
            .unwrap_or(100_000.0)
    };

    // ── gather subscriptions ────────────────────────────────────────────────
    let mut subscriptions: Vec<Arc<SubscriptionDataConfig>> =
        { adapter.inner.lock().unwrap().subscription_manager.get_all() };

    if subscriptions.is_empty() {
        warn!("No subscriptions — strategy did not call add_equity/add_forex.");
    }

    // ── determine effective benchmark ticker ────────────────────────────────
    // Use the symbol set by set_benchmark(), or fall back to SPY.
    let effective_benchmark_ticker: String = {
        adapter
            .inner
            .lock()
            .unwrap()
            .benchmark_symbol
            .clone()
            .unwrap_or_else(|| "SPY".to_string())
    };

    // Build a Symbol for the benchmark equity (daily resolution).
    // If the benchmark is already in the algorithm's subscriptions we reuse
    // that subscription's SID; otherwise we create an independent one and
    // load its data separately inside the date loop.
    let benchmark_symbol_obj: Symbol = {
        let market = lean_core::Market::usa();
        Symbol::create_equity(&effective_benchmark_ticker, &market)
    };
    let benchmark_in_subs: bool = subscriptions.iter().any(|s| {
        s.symbol
            .permtick
            .eq_ignore_ascii_case(&effective_benchmark_ticker)
    });

    info!(
        "Benchmark: {} ({})",
        effective_benchmark_ticker,
        if benchmark_in_subs {
            "already subscribed"
        } else {
            "internal subscription"
        }
    );

    // ── build infrastructure ────────────────────────────────────────────────
    let reader = Arc::new(ParquetReader::new());
    let resolver = PathResolver::new(config.data_root.clone());
    let cache = DataCache::new(50_000);
    let transactions = adapter.inner.lock().unwrap().transactions.clone();
    let portfolio = adapter.inner.lock().unwrap().portfolio.clone();

    let order_processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        transactions,
    );

    // ── determine warm-up window ────────────────────────────────────────────
    // Compute this before prefetching so data is requested once for the full
    // range the algorithm can consume. This mirrors LEAN's source-driven data
    // provider path and avoids overwriting daily/hourly single-file data with a
    // narrower main-period-only download.
    let warmup_start: Option<NaiveDate> = {
        let alg = adapter.inner.lock().unwrap();
        if let Some(bar_count) = alg.warmup_bar_count {
            // C# LEAN counts back N trading days using exchange calendar.
            // For daily data: 5 trading days per 7 calendar days -> multiply by 7/5.
            // Add a small buffer (+10) to ensure we never undershoot.
            let calendar_days = (bar_count as i64 * 7 + 4) / 5 + 10;
            Some(start_date - chrono::Duration::days(calendar_days))
        } else if let Some(dur) = alg.warmup_duration {
            let days = (dur.nanos / TimeSpan::ONE_DAY.nanos).max(1);
            Some(start_date - chrono::Duration::days(days))
        } else if let Some(period) = alg.warmup_period {
            let days = (period.nanos / TimeSpan::ONE_DAY.nanos).max(1);
            Some(start_date - chrono::Duration::days(days))
        } else {
            None
        }
    };

    // ── pre-fetch missing data ───────────────────────────────────────────────
    if let Some(ref provider) = config.historical_provider {
        pre_fetch_all(
            provider.clone(),
            config.history_provider.clone(),
            &subscriptions,
            warmup_start.unwrap_or(start_date),
            end_date,
            &resolver,
        )
        .await?;
    }

    // ── factor files: load from disk ─────────────────────────────────────────
    // Factor files are Parquet; key = symbol SID → rows sorted newest first.
    // Generated during pre_fetch_all via DataType::FactorFile requests —
    // providers that support corporate actions (e.g. massive) handle the
    // request; those that don't (e.g. thetadata) return NotImplemented.
    let factor_reader = ParquetReader::new();
    let mut factor_map: HashMap<u64, Vec<FactorFileEntry>> = HashMap::new();
    for sub in &subscriptions {
        let ticker = sub.symbol.permtick.to_lowercase();
        let market = sub.symbol.market().as_str().to_lowercase();
        let sec = format!("{}", sub.symbol.security_type()).to_lowercase();
        let factor_path = config
            .data_root
            .join(&sec)
            .join(&market)
            .join("factor_files")
            .join(format!("{ticker}.parquet"));

        match factor_reader.read_factor_file(&factor_path) {
            Ok(rows) if !rows.is_empty() => {
                info!("Loaded {} factor rows for {}", rows.len(), sub.symbol.value);
                factor_map.insert(sub.symbol.id.sid, rows);
            }
            _ => {
                warn!(
                    "Factor file missing for {} — bars will not be adjusted.",
                    sub.symbol.value
                );
            }
        }
    }

    // ── map files: load ticker rename history ───────────────────────────────
    // Map files are Parquet; key = symbol SID → rows sorted newest first.
    // Used to fire SymbolChangedEvent (rename) and Delisting events each day.
    let mut map_file_map: HashMap<u64, Vec<lean_storage::MapFileEntry>> = HashMap::new();
    for sub in &subscriptions {
        let ticker = sub.symbol.permtick.to_lowercase();
        let market = sub.symbol.market().as_str().to_lowercase();
        let map_path = config
            .data_root
            .join("equity")
            .join(&market)
            .join("map_files")
            .join(format!("{ticker}.parquet"));

        match factor_reader.read_map_file(&map_path) {
            Ok(rows) if !rows.is_empty() => {
                info!("Loaded {} map rows for {}", rows.len(), sub.symbol.value);
                map_file_map.insert(sub.symbol.id.sid, rows);
            }
            _ => {} // no map file = no rename/delist info; silent
        }
    }

    // ── option underlying SIDs: skip factor adjustment for these ─────────────
    // When a strategy subscribes to options, LEAN uses raw (unadjusted) prices
    // for the underlying equity so that strike selection matches live market
    // prices.  Build a set of equity SIDs that serve as option underlyings so
    // the bar-loading loop can bypass apply_factor_row for them.
    let option_underlying_sids: std::collections::HashSet<u64> = {
        let alg = adapter.inner.lock().unwrap();
        let mut sids = std::collections::HashSet::new();
        for canonical in &alg.option_subscriptions {
            let underlying_ticker = canonical.permtick.trim_start_matches('?');
            for sub in &subscriptions {
                if sub.symbol.permtick.eq_ignore_ascii_case(underlying_ticker) {
                    sids.insert(sub.symbol.id.sid);
                }
            }
        }
        sids
    };

    // ── exchange-hours filter map ─────────────────────────────────────────────
    // For each subscription with extended_market_hours=false, record the
    // ExchangeHours so the minute loop can drop pre-market / after-hours bars.
    // Keyed by SID; subscriptions with extended_market_hours=true are absent.
    let market_hours_filter: HashMap<u64, lean_core::exchange_hours::ExchangeHours> = {
        let alg = adapter.inner.lock().unwrap();
        let mut map = HashMap::new();
        for sub in &subscriptions {
            if !sub.extended_market_hours {
                if let Some(sec) = alg.securities.get(&sub.symbol) {
                    map.insert(sub.symbol.id.sid, sec.exchange_hours.clone());
                }
            }
        }
        map
    };

    // ── warm-up loop ────────────────────────────────────────────────────────
    if let Some(wu_start) = warmup_start {
        info!("Warm-up: {} → {} (exclusive)", wu_start, start_date);

        let mut wu_date = wu_start;
        while wu_date < start_date {
            let utc_time = date_to_datetime(wu_date, 16, 0, 0);
            set_algorithm_time(&adapter, utc_time);

            let mut slice = Slice::new(utc_time);
            for sub in &subscriptions {
                let sid = sub.symbol.id.sid;
                let day_key = day_key(wu_date);
                let path = resolver
                    .trade_bar(&sub.symbol, sub.resolution, wu_date)
                    .to_path();

                if path.exists() {
                    let bars = if let Some(cached) = cache.get_bars(sid, day_key) {
                        cached.as_ref().clone()
                    } else {
                        let day_start = date_to_datetime(wu_date, 0, 0, 0);
                        let day_end = date_to_datetime(wu_date, 23, 59, 59);
                        let params = QueryParams::new().with_time_range(day_start, day_end);
                        let loaded = reader
                            .read_trade_bars(&[path], sub.symbol.clone(), &params)
                            .await
                            .unwrap_or_default();
                        cache.insert_bars(sid, day_key, loaded.clone());
                        loaded
                    };

                    for bar in bars {
                        let bar = if let Some(rows) = factor_map.get(&sid) {
                            apply_factor_row(bar, rows, wu_date)
                        } else {
                            bar
                        };
                        adapter
                            .inner
                            .lock()
                            .unwrap()
                            .securities
                            .update_price(&bar.symbol, bar.close);
                        portfolio.update_prices(&bar.symbol, bar.close);
                        slice.add_bar(bar);
                    }
                }
            }

            if slice.has_data {
                // During warm-up: call on_data for indicator updates only.
                // Orders are NOT processed; equity is NOT recorded.
                adapter.on_data(&slice);
            }

            wu_date += chrono::Duration::days(1);
        }

        // Signal end of warm-up.
        adapter.inner.lock().unwrap().end_warm_up();
        adapter.on_warmup_finished();
        info!("Warm-up complete.");
    }

    // ── detect resolution mode ───────────────────────────────────────────────
    let is_intraday = subscriptions
        .iter()
        .any(|s| s.resolution.is_high_resolution());

    // ── pre-load all subscription bars (daily mode only) ─────────────────────
    // For daily (and other single-file) resolutions the same parquet file would
    // be opened and scanned once per trading day in the loop.  Pre-loading the
    // full date range up front reduces 629 file reads to 1 per subscription.
    //
    // bar_map and subscriptions are mut because strategies may call add_equity()
    // mid-backtest (dynamic universe selection).  New subscriptions are detected
    // at the start of each trading day and their bars are lazy-loaded here.
    let daily_full_params = QueryParams::new().with_time_range(
        date_to_datetime(start_date, 0, 0, 0),
        date_to_datetime(end_date, 23, 59, 59),
    );
    let mut bar_map: HashMap<u64, HashMap<chrono::NaiveDate, lean_data::TradeBar>> = if !is_intraday
    {
        let mut map = HashMap::new();
        for sub in &subscriptions {
            let sid = sub.symbol.id.sid;
            let path = resolver
                .trade_bar(&sub.symbol, sub.resolution, start_date)
                .to_path();
            if path.exists() {
                let bars = reader
                    .read_trade_bars(&[path], sub.symbol.clone(), &daily_full_params)
                    .await
                    .unwrap_or_default();
                let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> =
                    bars.into_iter().map(|b| (b.time.date_utc(), b)).collect();
                info!(
                    "Pre-loaded {} bars for {}",
                    date_map.len(),
                    sub.symbol.value
                );
                map.insert(sid, date_map);
            }
        }
        map
    } else {
        HashMap::new()
    };
    // Track which SIDs have been loaded to detect new dynamic subscriptions.
    let mut loaded_sids: std::collections::HashSet<u64> =
        subscriptions.iter().map(|s| s.symbol.id.sid).collect();

    // ── pre-allocate proxy objects for the hot path ──────────────────────────
    // One PyTradeBar per subscription is allocated here and reused every day.
    // `on_data_proxy` updates fields in-place instead of constructing new objects.
    let mut slice_proxy = Python::attach(|py| SliceProxy::new(py, &subscriptions))
        .context("Failed to create SliceProxy")?;

    // ── pre-load benchmark data ──────────────────────────────────────────────
    let benchmark_sid: u64 = benchmark_symbol_obj.id.sid;
    let mut benchmark_curve: Vec<Decimal> = Vec::new();
    let benchmark_price_map: HashMap<NaiveDate, Decimal> = {
        let mut map: HashMap<NaiveDate, Decimal> = HashMap::new();
        if !benchmark_in_subs {
            let bm_sym = benchmark_symbol_obj.clone();
            let day_start = date_to_datetime(start_date, 0, 0, 0);
            let day_end = date_to_datetime(end_date, 23, 59, 59);
            let params = QueryParams::new().with_time_range(day_start, day_end);
            let bm_path = resolver
                .trade_bar(&bm_sym, Resolution::Daily, start_date)
                .to_path();
            if bm_path.exists() {
                match reader
                    .read_trade_bars(&[bm_path], bm_sym.clone(), &params)
                    .await
                {
                    Ok(bars) => {
                        for b in bars {
                            let d = b.time.date_utc();
                            map.insert(d, b.close);
                        }
                        info!(
                            "Loaded {} benchmark price points for {}",
                            map.len(),
                            effective_benchmark_ticker
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Could not load benchmark data for {}: {} — proceeding without benchmark",
                            effective_benchmark_ticker, e
                        );
                    }
                }
            } else {
                warn!(
                    "Benchmark data file not found for {} at {} — proceeding without benchmark",
                    effective_benchmark_ticker,
                    resolver
                        .trade_bar(&bm_sym, Resolution::Daily, start_date)
                        .to_path()
                        .display()
                );
            }
        }
        map
    };

    // Trade tracking: open_positions maps symbol SID → (entry_time, entry_price, quantity).
    // When a fill closes a position we emit a completed Trade.
    let mut open_positions: HashMap<u64, (DateTime, Decimal, Decimal)> = HashMap::new();
    let mut completed_trades: Vec<Trade> = Vec::new();

    // Collect all order events emitted during the backtest.
    let mut all_order_events: Vec<OrderEvent> = Vec::new();

    // Data request tracking: record which symbol+date combinations had data and which did not.
    let mut succeeded_data_requests: Vec<String> = Vec::new();
    let mut failed_data_requests: Vec<String> = Vec::new();

    // Record the backtest start time as Unix epoch seconds (used as the LEAN backtest ID).
    let backtest_id = chrono::Utc::now().timestamp();
    let mut live_writer = config
        .output_dir
        .clone()
        .map(|dir| LiveBacktestWriter::new(dir, start_date, end_date));

    // ── prefetch full-history custom data sources ────────────────────────────
    // For sources where is_full_history_source() is true (FRED, CBOE VIX, …),
    // download the full series once, cache to history.parquet, and load the
    // entire series into memory so the loop can look up by date without any
    // per-day I/O or HTTP calls.
    //
    // Uses async reqwest (not blocking) so that HTTP/2 (required by some
    // providers like FRED) works correctly inside the tokio runtime.
    //
    // key: ticker (uppercased) → date → points for that date
    let custom_history: HashMap<String, HashMap<NaiveDate, Vec<CustomDataPoint>>> = {
        let subs: Vec<CustomDataSubscription> = adapter
            .inner
            .lock()
            .unwrap()
            .custom_data_subscriptions
            .clone();
        let mut out: HashMap<String, HashMap<NaiveDate, Vec<CustomDataPoint>>> = HashMap::new();

        for sub in &subs {
            let Some(source) = config
                .custom_data_sources
                .iter()
                .find(|s| s.name() == sub.source_type)
            else {
                continue;
            };
            if !source.is_full_history_source() {
                continue;
            }
            let history_path =
                custom_data_history_path(&config.data_root, &sub.source_type, &sub.ticker);

            // Try reading from existing on-disk cache first (synchronous, fast).
            let all_points: Vec<CustomDataPoint> = if history_path.exists() {
                let hp = history_path.clone();
                tokio::task::spawn_blocking(move || {
                    ParquetReader::new()
                        .read_custom_data_points(&hp)
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            } else {
                // Download full series using async HTTP.
                let data_source = match source.get_source(
                    &sub.ticker,
                    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
                    &sub.config,
                ) {
                    Some(s) => s,
                    None => {
                        warn!(
                            "custom data: get_source returned None for {}/{}",
                            sub.source_type, sub.ticker
                        );
                        continue;
                    }
                };
                let raw = match data_source.transport {
                    lean_data::custom::CustomDataTransport::Http => {
                        // Use curl subprocess: handles HTTP/2, TLS quirks, and redirects
                        // more reliably than reqwest in this environment (some servers
                        // like FRED require HTTP/2 which curl negotiates natively).
                        let output = tokio::process::Command::new("curl")
                            .args([
                                "-s",
                                "--max-time",
                                "120",
                                "-L", // follow redirects
                                &data_source.uri,
                            ])
                            .output()
                            .await;
                        match output {
                            Ok(out) if out.status.success() => {
                                String::from_utf8_lossy(&out.stdout).to_string()
                            }
                            Ok(out) => {
                                let stderr = String::from_utf8_lossy(&out.stderr);
                                warn!(
                                    "custom data full-history curl failed for {}/{}: {}",
                                    sub.source_type, sub.ticker, stderr
                                );
                                continue;
                            }
                            Err(e) => {
                                warn!(
                                    "custom data full-history download failed for {}/{}: {}",
                                    sub.source_type, sub.ticker, e
                                );
                                continue;
                            }
                        }
                    }
                    lean_data::custom::CustomDataTransport::LocalFile => {
                        match std::fs::read_to_string(&data_source.uri) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!(
                                    "custom data local file read failed for {}/{}: {}",
                                    sub.source_type, sub.ticker, e
                                );
                                continue;
                            }
                        }
                    }
                };
                // Parse all rows using the plugin (no date filter).
                let source_clone = source.clone();
                let cfg_clone = sub.config.clone();
                let pts: Vec<CustomDataPoint> = tokio::task::spawn_blocking(move || {
                    raw.lines()
                        .filter_map(|line| source_clone.read_history_line(line, &cfg_clone))
                        .collect()
                })
                .await
                .unwrap_or_default();

                if pts.is_empty() {
                    warn!(
                        "custom data: no points parsed for {}/{}",
                        sub.source_type, sub.ticker
                    );
                    continue;
                }
                // Cache to Parquet (off the async thread).
                let hp = history_path.clone();
                let pts_clone = pts.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(parent) = hp.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // Disable bloom filters AND page statistics: parquet-rs 53.x
                    // reader panics on TType::Set in metadata written with these
                    // features (read_set_begin is unimplemented). Custom data caches
                    // are always read fully so these features provide no benefit.
                    if let Err(e) = ParquetWriter::new(WriterConfig {
                        bloom_filter: false,
                        write_statistics: false,
                        ..WriterConfig::default()
                    })
                    .write_custom_data_points(&pts_clone, &hp)
                    {
                        warn!("custom data: failed to cache history: {}", e);
                    }
                })
                .await
                .ok();
                pts
            };

            info!(
                "custom data: loaded {} history points for {}/{}",
                all_points.len(),
                sub.source_type,
                sub.ticker
            );
            // Index by date.
            let mut by_date: HashMap<NaiveDate, Vec<CustomDataPoint>> = HashMap::new();
            for pt in all_points {
                by_date.entry(pt.time).or_default().push(pt);
            }
            out.insert(sub.ticker.to_uppercase(), by_date);
        }
        out
    };

    // ── date loop ───────────────────────────────────────────────────────────
    let sim_start = std::time::Instant::now();
    let mut current_date = start_date;
    let mut trading_days = 0i64;
    let mut equity_curve: Vec<Decimal> = Vec::new();
    let mut daily_dates: Vec<String> = Vec::new();

    let resolution_label = if subscriptions
        .iter()
        .any(|s| s.resolution == Resolution::Tick)
    {
        "tick"
    } else if is_intraday {
        "minute"
    } else {
        "daily"
    };
    info!(
        "Backtest: {} → {} ({})",
        start_date, end_date, resolution_label
    );

    while current_date <= end_date {
        if is_intraday {
            // ── INTRADAY LOOP ────────────────────────────────────────────────
            let mut day_trade_bars: HashMap<u64, Vec<TradeBar>> = HashMap::new();
            let mut day_quote_bars: HashMap<u64, Vec<QuoteBar>> = HashMap::new();
            let mut day_ticks: HashMap<u64, Vec<Tick>> = HashMap::new();

            let day_params = QueryParams::new().with_time_range(
                date_to_datetime(current_date, 0, 0, 0),
                date_to_datetime(current_date, 23, 59, 59),
            );

            for sub in &subscriptions {
                // In intraday mode skip low-resolution subscriptions (e.g. the daily
                // underlying equity subscription that add_option() creates internally).
                // They share the same SID as the high-resolution subscription and
                // would overwrite the correct minute bars with daily bars.
                if !sub.resolution.is_high_resolution() {
                    continue;
                }
                if !is_expected_market_date(&sub.symbol, current_date) {
                    continue;
                }

                let sid = sub.symbol.id.sid;
                let mut had_data = false;

                if sub.resolution == Resolution::Tick {
                    let tick_path = resolver.tick(&sub.symbol, current_date).to_path();
                    if !tick_path.exists() {
                        if let Some(ref provider) = config.historical_provider {
                            if let Err(e) = pre_fetch_subscription(
                                provider.clone(),
                                config.history_provider.clone(),
                                sub.clone(),
                                current_date,
                                current_date,
                                resolver.clone(),
                            )
                            .await
                            {
                                warn!(
                                    "Failed to fetch data for {} on {}: {}",
                                    sub.symbol.value, current_date, e
                                );
                            }
                        }
                    }
                    if tick_path.exists() {
                        match reader.read_ticks(&[tick_path], sub.symbol.clone(), &day_params) {
                            Ok(mut ticks) if !ticks.is_empty() => {
                                if let Some(hours) = market_hours_filter.get(&sid) {
                                    ticks.retain(|tick| hours.is_open_at(tick.time));
                                }
                                ticks.retain(|tick| match tick.tick_type {
                                    TickType::Trade => tick.value > Decimal::ZERO,
                                    TickType::Quote => {
                                        tick.bid_price > Decimal::ZERO
                                            || tick.ask_price > Decimal::ZERO
                                    }
                                    TickType::OpenInterest => true,
                                });
                                if !ticks.is_empty() {
                                    had_data = true;
                                    day_ticks.insert(sid, ticks);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!(
                                "Failed to read ticks for {} on {}: {}",
                                sub.symbol.value, current_date, e
                            ),
                        }
                    }
                } else {
                    let trade_path = resolver
                        .trade_bar(&sub.symbol, sub.resolution, current_date)
                        .to_path();
                    if !trade_path.exists() {
                        if let Some(ref provider) = config.historical_provider {
                            if let Err(e) = pre_fetch_subscription(
                                provider.clone(),
                                config.history_provider.clone(),
                                sub.clone(),
                                current_date,
                                current_date,
                                resolver.clone(),
                            )
                            .await
                            {
                                warn!(
                                    "Failed to fetch data for {} on {}: {}",
                                    sub.symbol.value, current_date, e
                                );
                            }
                        }
                    }
                    if trade_path.exists() {
                        match reader
                            .read_trade_bars(&[trade_path], sub.symbol.clone(), &day_params)
                            .await
                        {
                            Ok(mut bars) if !bars.is_empty() => {
                                if let Some(hours) = market_hours_filter.get(&sid) {
                                    bars.retain(|bar| hours.is_open_at(bar.time));
                                }
                                bars.retain(|bar| bar.close > Decimal::ZERO);
                                if !bars.is_empty() {
                                    had_data = true;
                                    day_trade_bars.insert(sid, bars);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!(
                                "Failed to read intraday bars for {} on {}: {}",
                                sub.symbol.value, current_date, e
                            ),
                        }
                    }

                    let quote_path = resolver
                        .quote_bar(&sub.symbol, sub.resolution, current_date)
                        .to_path();
                    if quote_path.exists() {
                        match reader.read_quote_bars(&[quote_path], sub.symbol.clone(), &day_params)
                        {
                            Ok(mut bars) if !bars.is_empty() => {
                                if let Some(hours) = market_hours_filter.get(&sid) {
                                    bars.retain(|bar| hours.is_open_at(bar.time));
                                }
                                bars.retain(|bar| bar.mid_close() > Decimal::ZERO);
                                if !bars.is_empty() {
                                    had_data = true;
                                    day_quote_bars.insert(sid, bars);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!(
                                "Failed to read quote bars for {} on {}: {}",
                                sub.symbol.value, current_date, e
                            ),
                        }
                    }
                }

                if had_data {
                    succeeded_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                } else {
                    failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                }
            }

            let option_subs: Vec<Symbol> =
                { adapter.inner.lock().unwrap().option_subscriptions.clone() };
            let mut option_runtimes: Vec<OptionChainRuntime> = Vec::new();
            for canonical in &option_subs {
                let underlying_ticker = canonical.permtick.trim_start_matches('?');
                let option_resolution = subscriptions
                    .iter()
                    .find(|sub| {
                        sub.resolution.is_high_resolution()
                            && sub.symbol.permtick.eq_ignore_ascii_case(underlying_ticker)
                    })
                    .map(|sub| sub.resolution)
                    .unwrap_or(Resolution::Minute);
                let spot = {
                    adapter
                        .inner
                        .lock()
                        .unwrap()
                        .securities
                        .all()
                        .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                        .map(|s| s.current_price())
                        .unwrap_or(Decimal::ZERO)
                };
                let ticker = underlying_ticker.to_uppercase();
                let runtime = tokio::task::spawn_blocking({
                    let provider = config.history_provider.clone();
                    let data_root = config.data_root.clone();
                    let canonical = canonical.clone();
                    move || {
                        load_option_chain_runtime(
                            &data_root,
                            &ticker,
                            &canonical,
                            option_resolution,
                            current_date,
                            spot,
                            provider.as_ref(),
                        )
                    }
                })
                .await
                .unwrap_or_else(|_| OptionChainRuntime {
                    permtick: canonical.permtick.clone(),
                    chain: OptionChain::new(canonical.clone(), spot),
                    trade_updates: HashMap::new(),
                    quote_updates: HashMap::new(),
                    tick_updates: HashMap::new(),
                });
                option_runtimes.push(runtime);
            }

            let mut all_timestamps: std::collections::BTreeSet<i64> =
                std::collections::BTreeSet::new();
            for bars in day_trade_bars.values() {
                for bar in bars {
                    all_timestamps.insert(bar.time.0);
                }
            }
            for bars in day_quote_bars.values() {
                for bar in bars {
                    all_timestamps.insert(bar.time.0);
                }
            }
            for ticks in day_ticks.values() {
                for tick in ticks {
                    all_timestamps.insert(tick.time.0);
                }
            }
            for runtime in &option_runtimes {
                all_timestamps.extend(runtime.timestamps());
            }

            let has_data = !all_timestamps.is_empty();

            if has_data {
                trading_days += 1;

                let mut trade_by_ts: HashMap<u64, HashMap<i64, TradeBar>> = HashMap::new();
                for (&sid, bars) in &day_trade_bars {
                    let mut ts_map: HashMap<i64, TradeBar> = HashMap::new();
                    for bar in bars {
                        ts_map.insert(bar.time.0, bar.clone());
                    }
                    trade_by_ts.insert(sid, ts_map);
                }

                let mut quote_by_ts: HashMap<u64, HashMap<i64, QuoteBar>> = HashMap::new();
                for (&sid, qbars) in &day_quote_bars {
                    let mut ts_map: HashMap<i64, QuoteBar> = HashMap::new();
                    for q in qbars {
                        ts_map.insert(q.time.0, q.clone());
                    }
                    quote_by_ts.insert(sid, ts_map);
                }
                let mut ticks_by_ts: HashMap<u64, HashMap<i64, Vec<Tick>>> = HashMap::new();
                for (&sid, ticks) in &day_ticks {
                    let mut ts_map: HashMap<i64, Vec<Tick>> = HashMap::new();
                    for tick in ticks {
                        ts_map.entry(tick.time.0).or_default().push(tick.clone());
                    }
                    ticks_by_ts.insert(sid, ts_map);
                }

                let custom_subs: Vec<CustomDataSubscription> = {
                    adapter
                        .inner
                        .lock()
                        .unwrap()
                        .custom_data_subscriptions
                        .clone()
                };
                let mut custom_data_for_day: HashMap<String, Vec<CustomDataPoint>> = HashMap::new();
                for sub in &custom_subs {
                    if sub.config.resolution.is_high_resolution() {
                        continue;
                    }
                    let key = sub.ticker.to_uppercase();
                    if let Some(by_date) = custom_history.get(&key) {
                        // Full-history source: look up from preloaded in-memory map.
                        if let Some(pts) = by_date.get(&current_date) {
                            custom_data_for_day.insert(sub.ticker.clone(), pts.clone());
                        }
                    } else {
                        // Date-keyed source: per-day HTTP fetch with per-day Parquet cache.
                        let source = config
                            .custom_data_sources
                            .iter()
                            .find(|s| s.name() == sub.source_type)
                            .cloned();
                        let data_root = config.data_root.clone();
                        let source_type = sub.source_type.clone();
                        let ticker = sub.ticker.clone();
                        let cfg = sub.config.clone();
                        let dynamic_query = sub.dynamic_query.clone();
                        let points = load_custom_data_points_for_subscription(
                            data_root,
                            source_type,
                            ticker,
                            current_date,
                            source,
                            cfg,
                            dynamic_query,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "failed to load custom data for {}/{} {}",
                                sub.source_type, sub.ticker, current_date
                            )
                        })?;
                        if !points.is_empty() {
                            custom_data_for_day.insert(sub.ticker.clone(), points);
                        }
                    }
                }

                for &ts_ns in &all_timestamps {
                    let utc_time = lean_core::NanosecondTimestamp(ts_ns);
                    set_algorithm_time(&adapter, utc_time);

                    let mut minute_slice = Slice::new(utc_time);
                    let mut minute_quote_bars: HashMap<u64, QuoteBar> = HashMap::new();
                    let mut minute_ticks: HashMap<u64, Vec<Tick>> = HashMap::new();
                    let mut bars_for_orders: HashMap<u64, TradeBar> = HashMap::new();

                    for sub in &subscriptions {
                        let sid = sub.symbol.id.sid;
                        if let Some(raw_bar) = trade_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            let bar = if !option_underlying_sids.contains(&sid) {
                                if let Some(rows) = factor_map.get(&sid) {
                                    apply_factor_row(raw_bar.clone(), rows, current_date)
                                } else {
                                    raw_bar.clone()
                                }
                            } else {
                                raw_bar.clone()
                            };
                            adapter
                                .inner
                                .lock()
                                .unwrap()
                                .securities
                                .update_price(&bar.symbol, bar.close);
                            portfolio.update_prices(&bar.symbol, bar.close);
                            bars_for_orders.insert(sid, bar.clone());
                            minute_slice.add_bar(bar);
                        }

                        if let Some(qbar) = quote_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            let qbar = qbar.clone();
                            let mid = qbar.mid_close();
                            if mid > Decimal::ZERO && !bars_for_orders.contains_key(&sid) {
                                adapter
                                    .inner
                                    .lock()
                                    .unwrap()
                                    .securities
                                    .update_price(&qbar.symbol, mid);
                                portfolio.update_prices(&qbar.symbol, mid);
                            }
                            if let std::collections::hash_map::Entry::Vacant(e) =
                                bars_for_orders.entry(sid)
                            {
                                if let Some(synth) = synthesize_trade_bar_from_quote_bar(&qbar) {
                                    e.insert(synth);
                                }
                            }
                            minute_quote_bars.insert(sid, qbar.clone());
                            minute_slice.add_quote_bar(qbar);
                        }

                        if let Some(ticks) = ticks_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            if !ticks.is_empty() {
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    bars_for_orders.entry(sid)
                                {
                                    if let Some(synth) = synthesize_trade_bar_from_ticks(
                                        &sub.symbol,
                                        utc_time,
                                        ticks,
                                    ) {
                                        adapter
                                            .inner
                                            .lock()
                                            .unwrap()
                                            .securities
                                            .update_price(&synth.symbol, synth.close);
                                        portfolio.update_prices(&synth.symbol, synth.close);
                                        e.insert(synth);
                                    }
                                }
                                for tick in ticks {
                                    minute_slice.add_tick(tick.clone());
                                }
                                minute_ticks.insert(sid, ticks.clone());
                            }
                        }
                    }

                    for runtime in &mut option_runtimes {
                        let underlying_ticker = runtime.permtick.trim_start_matches('?');
                        let spot = {
                            adapter
                                .inner
                                .lock()
                                .unwrap()
                                .securities
                                .all()
                                .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                                .map(|s| s.current_price())
                                .unwrap_or(Decimal::ZERO)
                        };
                        runtime.apply_timestamp(utc_time, spot);
                    }

                    let chains_snapshot: Vec<(String, OptionChain)> = option_runtimes
                        .iter()
                        .map(|runtime| (runtime.permtick.clone(), runtime.chain.clone()))
                        .collect();
                    {
                        let mut alg = adapter.inner.lock().unwrap();
                        alg.option_chains.clear();
                        for (permtick, chain) in &chains_snapshot {
                            alg.option_chains.insert(permtick.clone(), chain.clone());
                        }
                    }
                    sync_option_holdings_to_chain_prices(&adapter, &portfolio, &chains_snapshot);

                    Python::attach(|py| {
                        adapter.apply_universe_selection(py, utc_time.0, Resolution::Minute);
                    });

                    let mut custom_data_for_slice = custom_data_for_day.clone();
                    for sub in custom_subs
                        .iter()
                        .filter(|sub| sub.config.resolution.is_high_resolution())
                    {
                        let source = config
                            .custom_data_sources
                            .iter()
                            .find(|s| s.name() == sub.source_type)
                            .cloned();
                        let timestamp_query = sub.dynamic_query.merge(&CustomDataQuery {
                            start_time: Some(utc_time),
                            end_time: Some(utc_time),
                            ..Default::default()
                        });
                        let points = load_custom_data_points_for_subscription(
                            config.data_root.clone(),
                            sub.source_type.clone(),
                            sub.ticker.clone(),
                            current_date,
                            source,
                            sub.config.clone(),
                            timestamp_query,
                        )
                        .await
                        .with_context(|| {
                            format!(
                                "failed to load custom data for {}/{} {} at {}",
                                sub.source_type, sub.ticker, current_date, utc_time.0
                            )
                        })?;
                        if !points.is_empty() {
                            custom_data_for_slice.insert(sub.ticker.clone(), points);
                        }
                    }

                    Python::attach(|py| {
                        slice_proxy.update_option_chains(py, &chains_snapshot);
                        slice_proxy.update_quote_bars(py, &minute_quote_bars);
                        slice_proxy.update_ticks(py, &minute_ticks);
                        slice_proxy.update_custom_data(py, &custom_data_for_slice);
                        adapter.on_data_proxy(py, &slice_proxy, &minute_slice);
                    });

                    // Mirror C# LEAN's DataManager.AddSubscription behavior for
                    // intraday universes: symbols added during this slice are
                    // attached to the active data stream so later slices can
                    // receive bars without requiring a static universe.
                    let current_subs =
                        { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                    for sub in &current_subs {
                        let sid = sub.symbol.id.sid;
                        if loaded_sids.contains(&sid) {
                            continue;
                        }
                        loaded_sids.insert(sid);
                        subscriptions.push(sub.clone());
                        let _ = Python::attach(|py| slice_proxy.add_subscription(py, sub));

                        if !sub.resolution.is_high_resolution() {
                            continue;
                        }

                        if let Some(ref provider) = config.historical_provider {
                            if let Err(e) = pre_fetch_subscription(
                                provider.clone(),
                                config.history_provider.clone(),
                                sub.clone(),
                                current_date,
                                current_date,
                                resolver.clone(),
                            )
                            .await
                            {
                                warn!(
                                    "Failed to fetch data for dynamic subscription {}: {}",
                                    sub.symbol.value, e
                                );
                            }
                        }

                        let ticker = sub.symbol.permtick.to_lowercase();
                        let market = sub.symbol.market().as_str().to_lowercase();
                        let sec = format!("{}", sub.symbol.security_type()).to_lowercase();
                        let factor_path = config
                            .data_root
                            .join(&sec)
                            .join(&market)
                            .join("factor_files")
                            .join(format!("{ticker}.parquet"));
                        if let Ok(rows) = factor_reader.read_factor_file(&factor_path) {
                            if !rows.is_empty() {
                                factor_map.insert(sid, rows);
                            }
                        }

                        let exchange_hours = {
                            adapter
                                .inner
                                .lock()
                                .unwrap()
                                .securities
                                .get(&sub.symbol)
                                .map(|s| s.exchange_hours.clone())
                        };

                        if sub.resolution == Resolution::Tick {
                            let tick_path = resolver.tick(&sub.symbol, current_date).to_path();
                            if tick_path.exists() {
                                match reader.read_ticks(
                                    &[tick_path],
                                    sub.symbol.clone(),
                                    &day_params,
                                ) {
                                    Ok(mut ticks) if !ticks.is_empty() => {
                                        if let Some(hours) = &exchange_hours {
                                            ticks.retain(|tick| hours.is_open_at(tick.time));
                                        }
                                        ticks.retain(|tick| match tick.tick_type {
                                            TickType::Trade => tick.value > Decimal::ZERO,
                                            TickType::Quote => {
                                                tick.bid_price > Decimal::ZERO
                                                    || tick.ask_price > Decimal::ZERO
                                            }
                                            TickType::OpenInterest => true,
                                        });
                                        if !ticks.is_empty() {
                                            let mut ts_map: HashMap<i64, Vec<Tick>> =
                                                HashMap::new();
                                            for tick in &ticks {
                                                ts_map
                                                    .entry(tick.time.0)
                                                    .or_default()
                                                    .push(tick.clone());
                                            }
                                            ticks_by_ts.insert(sid, ts_map);
                                            day_ticks.insert(sid, ticks);
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(
                                        "Failed to read dynamic ticks for {} on {}: {}",
                                        sub.symbol.value, current_date, e
                                    ),
                                }
                            }
                        } else {
                            let trade_path = resolver
                                .trade_bar(&sub.symbol, sub.resolution, current_date)
                                .to_path();
                            if trade_path.exists() {
                                match reader
                                    .read_trade_bars(&[trade_path], sub.symbol.clone(), &day_params)
                                    .await
                                {
                                    Ok(mut bars) if !bars.is_empty() => {
                                        if let Some(hours) = &exchange_hours {
                                            bars.retain(|bar| hours.is_open_at(bar.time));
                                        }
                                        bars.retain(|bar| bar.close > Decimal::ZERO);
                                        if !bars.is_empty() {
                                            let mut ts_map: HashMap<i64, TradeBar> = HashMap::new();
                                            for bar in &bars {
                                                ts_map.insert(bar.time.0, bar.clone());
                                            }
                                            trade_by_ts.insert(sid, ts_map);
                                            day_trade_bars.insert(sid, bars);
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(
                                        "Failed to read dynamic intraday bars for {} on {}: {}",
                                        sub.symbol.value, current_date, e
                                    ),
                                }
                            }

                            let quote_path = resolver
                                .quote_bar(&sub.symbol, sub.resolution, current_date)
                                .to_path();
                            if quote_path.exists() {
                                match reader.read_quote_bars(
                                    &[quote_path],
                                    sub.symbol.clone(),
                                    &day_params,
                                ) {
                                    Ok(mut bars) if !bars.is_empty() => {
                                        if let Some(hours) = &exchange_hours {
                                            bars.retain(|bar| hours.is_open_at(bar.time));
                                        }
                                        bars.retain(|bar| bar.mid_close() > Decimal::ZERO);
                                        if !bars.is_empty() {
                                            let mut ts_map: HashMap<i64, QuoteBar> = HashMap::new();
                                            for qbar in &bars {
                                                ts_map.insert(qbar.time.0, qbar.clone());
                                            }
                                            quote_by_ts.insert(sid, ts_map);
                                            day_quote_bars.insert(sid, bars);
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(
                                        "Failed to read dynamic quote bars for {} on {}: {}",
                                        sub.symbol.value, current_date, e
                                    ),
                                }
                            }
                        }
                    }

                    // ── Algorithm Framework pipeline (intraday) ───────────
                    {
                        let order_requests = run_framework_pipeline(
                            &adapter.framework,
                            &adapter.inner,
                            &minute_slice,
                        );
                        if !order_requests.is_empty() {
                            let mut alg = adapter.inner.lock().unwrap();
                            for req in order_requests {
                                use lean_execution::ExecutionOrderType;
                                match req.order_type {
                                    ExecutionOrderType::Market => {
                                        alg.market_order(&req.symbol, req.quantity);
                                    }
                                    ExecutionOrderType::Limit => {
                                        if let Some(lp) = req.limit_price {
                                            alg.limit_order(&req.symbol, req.quantity, lp);
                                        }
                                    }
                                    ExecutionOrderType::MarketOnOpen => {
                                        alg.market_on_open_order(&req.symbol, req.quantity);
                                    }
                                    ExecutionOrderType::MarketOnClose => {
                                        alg.market_on_close_order(&req.symbol, req.quantity);
                                    }
                                }
                            }
                        }
                    }

                    let fill_events = order_processor.process_orders(&bars_for_orders, utc_time);
                    all_order_events.extend(fill_events.iter().cloned());
                    if let Some(writer) = &live_writer {
                        writer.append_order_events(&fill_events);
                    }
                    let ib_fee_model = InteractiveBrokersFeeModel::default();
                    for event in &fill_events {
                        if event.is_fill() {
                            if let Some(order) = order_processor
                                .transaction_manager
                                .get_order(event.order_id)
                            {
                                let fee = ib_fee_model
                                    .get_order_fee(&OrderFeeParameters::equity(
                                        &order,
                                        event.fill_price,
                                    ))
                                    .amount;
                                let contract_multiplier = adapter
                                    .inner
                                    .lock()
                                    .unwrap()
                                    .securities
                                    .get(&order.symbol)
                                    .and_then(|sec| {
                                        Decimal::from_f64_retain(
                                            sec.symbol_properties.contract_multiplier,
                                        )
                                    })
                                    .unwrap_or_else(|| {
                                        if order.symbol.option_symbol_id().is_some() {
                                            rust_decimal_macros::dec!(100)
                                        } else {
                                            Decimal::ONE
                                        }
                                    });
                                portfolio.apply_fill_with_multiplier(
                                    &order.symbol,
                                    event.fill_price,
                                    event.fill_quantity,
                                    fee,
                                    contract_multiplier,
                                );
                            }
                            let sid = event.symbol.id.sid;
                            let fill_qty = event.fill_quantity;
                            if let Some((entry_time, entry_price, open_qty)) =
                                open_positions.remove(&sid)
                            {
                                let close_qty = open_qty.abs().min(fill_qty.abs());
                                let trade = Trade::new(
                                    event.symbol.clone(),
                                    entry_time,
                                    event.utc_time,
                                    entry_price,
                                    event.fill_price,
                                    close_qty,
                                    rust_decimal_macros::dec!(0),
                                );
                                if let Some(writer) = &live_writer {
                                    writer.append_trades(std::slice::from_ref(&trade));
                                }
                                completed_trades.push(trade);
                            } else {
                                open_positions
                                    .insert(sid, (event.utc_time, event.fill_price, fill_qty));
                            }
                        }
                        adapter.on_order_event(event);
                    }
                }

                // End-of-day calls.
                adapter.on_end_of_day(None);
                process_option_expirations(&mut adapter, current_date, &HashMap::new());

                let bm_close: Option<Decimal> = if benchmark_in_subs {
                    day_trade_bars
                        .get(&benchmark_sid)
                        .and_then(|bars| bars.last())
                        .map(|b| b.close)
                        .or_else(|| {
                            day_quote_bars
                                .get(&benchmark_sid)
                                .and_then(|bars| bars.last())
                                .map(|bar| bar.mid_close())
                        })
                        .or_else(|| {
                            day_ticks.get(&benchmark_sid).and_then(|ticks| {
                                ticks.iter().rev().find_map(|tick| match tick.tick_type {
                                    TickType::Trade if tick.value > Decimal::ZERO => {
                                        Some(tick.value)
                                    }
                                    TickType::Quote if tick.value > Decimal::ZERO => {
                                        Some(tick.value)
                                    }
                                    _ => None,
                                })
                            })
                        })
                } else {
                    benchmark_price_map.get(&current_date).copied()
                };
                if let Some(close) = bm_close {
                    benchmark_curve.push(close);
                }

                let day_equity = portfolio.total_portfolio_value();
                equity_curve.push(day_equity);
                daily_dates.push(current_date.to_string());
                if let Some(writer) = &mut live_writer {
                    writer.record_progress(
                        current_date,
                        trading_days,
                        day_equity,
                        all_order_events.len(),
                        completed_trades.len(),
                    );
                }
            }

            current_date += chrono::Duration::days(1);
        } else {
            // ── DAILY LOOP ───────────────────────────────────────────────────

            // ── lazy-load bars for dynamically added subscriptions ────────────
            // Strategies may call add_equity() mid-backtest (universe selection).
            // Detect new subscriptions and load their full bar history so that
            // security prices are available when set_holdings() is called.
            if !is_intraday {
                let current_subs = { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                for sub in &current_subs {
                    let sid = sub.symbol.id.sid;
                    if !loaded_sids.contains(&sid) {
                        loaded_sids.insert(sid);
                        let path = resolver
                            .trade_bar(&sub.symbol, sub.resolution, start_date)
                            .to_path();
                        // If the bar file isn't cached locally, fetch it now from
                        // the historical provider (same as pre_fetch_all does at startup).
                        if !path.exists() {
                            if let Some(ref provider) = config.historical_provider {
                                if let Err(e) = pre_fetch_all(
                                    provider.clone(),
                                    config.history_provider.clone(),
                                    std::slice::from_ref(sub),
                                    start_date,
                                    end_date,
                                    &resolver,
                                )
                                .await
                                {
                                    warn!(
                                        "Failed to fetch data for dynamic subscription {}: {}",
                                        sub.symbol.value, e
                                    );
                                }
                            }
                        }
                        if path.exists() {
                            let bars = reader
                                .read_trade_bars(&[path], sub.symbol.clone(), &daily_full_params)
                                .await
                                .unwrap_or_default();
                            let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> =
                                bars.into_iter().map(|b| (b.time.date_utc(), b)).collect();
                            if !date_map.is_empty() {
                                bar_map.insert(sid, date_map);
                            }
                        }
                        subscriptions.push(sub.clone());
                    }
                }
            }

            let utc_time = date_to_datetime(current_date, 16, 0, 0);
            set_algorithm_time(&adapter, utc_time);

            Python::attach(|py| {
                adapter.apply_universe_selection(py, utc_time.0, Resolution::Daily);
            });

            // Universe selection can add subscriptions at the frontier before
            // OnData. Load those subscriptions now so the current slice can
            // contain their data, matching C# LEAN's time-pulse selection flow.
            if !is_intraday {
                let current_subs = { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                for sub in &current_subs {
                    let sid = sub.symbol.id.sid;
                    if !loaded_sids.contains(&sid) {
                        loaded_sids.insert(sid);
                        let path = resolver
                            .trade_bar(&sub.symbol, sub.resolution, start_date)
                            .to_path();
                        if !path.exists() {
                            if let Some(ref provider) = config.historical_provider {
                                if let Err(e) = pre_fetch_all(
                                    provider.clone(),
                                    config.history_provider.clone(),
                                    std::slice::from_ref(sub),
                                    start_date,
                                    end_date,
                                    &resolver,
                                )
                                .await
                                {
                                    warn!(
                                        "Failed to fetch data for universe subscription {}: {}",
                                        sub.symbol.value, e
                                    );
                                }
                            }
                        }
                        if path.exists() {
                            let bars = reader
                                .read_trade_bars(&[path], sub.symbol.clone(), &daily_full_params)
                                .await
                                .unwrap_or_default();
                            let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> =
                                bars.into_iter().map(|b| (b.time.date_utc(), b)).collect();
                            if !date_map.is_empty() {
                                bar_map.insert(sid, date_map);
                            }
                        }
                        subscriptions.push(sub.clone());
                        let _ = Python::attach(|py| slice_proxy.add_subscription(py, sub));
                    }
                }
            }

            // ── split adjustment for option-underlying equities ───────────────
            // Option-underlying equities skip factor adjustment (raw prices used
            // so that the strategy sees prices on the same scale as ThetaData
            // option strikes).  When a split occurs, the raw price halves but
            // holding quantities don't change — causing a 50% portfolio cliff.
            //
            // Fix: detect when the split_factor boundary crosses into a new era
            // and adjust holding quantity / average_price exactly as C# LEAN's
            // SecurityPortfolioManager.ApplySplit does in live/raw mode.
            //
            // Also record split ratios for use by process_option_expirations so
            // that option contracts written in the pre-split era are correctly
            // evaluated OTM/ITM against the post-split spot price.
            let mut split_ratios_today: HashMap<u64, f64> = HashMap::new();
            for sid in &option_underlying_sids {
                if let Some(rows) = factor_map.get(sid) {
                    let (_, sf_today) = factor_for_entry(rows, current_date);
                    let prev_date = current_date - chrono::Duration::days(1);
                    let (_, sf_prev) = factor_for_entry(rows, prev_date);
                    if (sf_today - sf_prev).abs() > 1e-9 && sf_prev > 0.0 {
                        // Split factor changed — adjust equity holding quantities.
                        // ratio > 1 = forward split (more shares at lower price).
                        let ratio = sf_today / sf_prev;
                        split_ratios_today.insert(*sid, ratio);
                        let holding = portfolio.get_holding_by_sid(*sid);
                        if holding.is_invested() {
                            let new_qty =
                                holding.quantity * Decimal::from_f64(ratio).unwrap_or(Decimal::ONE);
                            let new_avg = if ratio != 0.0 {
                                holding.average_price
                                    / Decimal::from_f64(ratio).unwrap_or(Decimal::ONE)
                            } else {
                                holding.average_price
                            };
                            portfolio.set_holdings(
                                &holding.symbol,
                                new_avg,
                                new_qty,
                                holding.contract_multiplier,
                            );
                            info!(
                                "Split adjustment: {} sf {:.4}→{:.4} (×{:.4}): \
                                 qty {:.0}→{:.0} avg_px {:.4}→{:.4}",
                                holding.symbol.value,
                                sf_prev,
                                sf_today,
                                ratio,
                                holding.quantity.to_f64().unwrap_or(0.0),
                                new_qty.to_f64().unwrap_or(0.0),
                                holding.average_price.to_f64().unwrap_or(0.0),
                                new_avg.to_f64().unwrap_or(0.0),
                            );
                        }
                    }
                }
            }

            let mut slice = Slice::new(utc_time);
            for sub in &subscriptions {
                let sid = sub.symbol.id.sid;
                if let Some(day_bar) = bar_map.get(&sid).and_then(|m| m.get(&current_date)) {
                    let bar = if !option_underlying_sids.contains(&sid) {
                        if let Some(rows) = factor_map.get(&sid) {
                            apply_factor_row(day_bar.clone(), rows, current_date)
                        } else {
                            day_bar.clone()
                        }
                    } else {
                        day_bar.clone()
                    };
                    adapter
                        .inner
                        .lock()
                        .unwrap()
                        .securities
                        .update_price(&bar.symbol, bar.close);
                    portfolio.update_prices(&bar.symbol, bar.close);
                    succeeded_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                    slice.add_bar(bar);
                } else {
                    failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                }
            }

            if !slice.has_data {
                current_date += chrono::Duration::days(1);
                continue;
            }

            // Record benchmark close for this trading day.
            let bm_close: Option<Decimal> = if benchmark_in_subs {
                slice.bars.get(&benchmark_sid).map(|b| b.close)
            } else {
                benchmark_price_map.get(&current_date).copied()
            };
            if let Some(close) = bm_close {
                benchmark_curve.push(close);
            }

            trading_days += 1;

            let bars_map: HashMap<u64, lean_data::TradeBar> =
                slice.bars.iter().map(|(&k, v)| (k, v.clone())).collect();

            let fill_events = order_processor.process_orders(&bars_map, utc_time);
            all_order_events.extend(fill_events.iter().cloned());
            if let Some(writer) = &live_writer {
                writer.append_order_events(&fill_events);
            }
            let ib_fee_model = InteractiveBrokersFeeModel::default();
            for event in &fill_events {
                if event.is_fill() {
                    if let Some(order) = order_processor
                        .transaction_manager
                        .get_order(event.order_id)
                    {
                        let fee = ib_fee_model
                            .get_order_fee(&OrderFeeParameters::equity(&order, event.fill_price))
                            .amount;
                        let contract_multiplier = adapter
                            .inner
                            .lock()
                            .unwrap()
                            .securities
                            .get(&order.symbol)
                            .and_then(|sec| {
                                Decimal::from_f64_retain(sec.symbol_properties.contract_multiplier)
                            })
                            .unwrap_or_else(|| {
                                if order.symbol.option_symbol_id().is_some() {
                                    rust_decimal_macros::dec!(100)
                                } else {
                                    Decimal::ONE
                                }
                            });
                        portfolio.apply_fill_with_multiplier(
                            &order.symbol,
                            event.fill_price,
                            event.fill_quantity,
                            fee,
                            contract_multiplier,
                        );
                    }

                    let sid = event.symbol.id.sid;
                    let fill_qty = event.fill_quantity;
                    if let Some((entry_time, entry_price, open_qty)) = open_positions.remove(&sid) {
                        let close_qty = open_qty.abs().min(fill_qty.abs());
                        let trade = Trade::new(
                            event.symbol.clone(),
                            entry_time,
                            event.utc_time,
                            entry_price,
                            event.fill_price,
                            close_qty,
                            rust_decimal_macros::dec!(0),
                        );
                        if let Some(writer) = &live_writer {
                            writer.append_trades(std::slice::from_ref(&trade));
                        }
                        completed_trades.push(trade);
                    } else {
                        open_positions.insert(sid, (event.utc_time, event.fill_price, fill_qty));
                    }
                }
                adapter.on_order_event(event);
            }

            // Build option chains before calling on_data.
            {
                let option_subs: Vec<Symbol> =
                    { adapter.inner.lock().unwrap().option_subscriptions.clone() };

                let mut chains_for_day: Vec<(String, OptionChain)> = Vec::new();
                for canonical in &option_subs {
                    let underlying_ticker = canonical.permtick.trim_start_matches('?');
                    let spot = {
                        adapter
                            .inner
                            .lock()
                            .unwrap()
                            .securities
                            .all()
                            .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                            .map(|s| s.current_price())
                            .unwrap_or(Decimal::ZERO)
                    };

                    let chain = {
                        let ticker = underlying_ticker.to_uppercase();
                        let bars = tokio::task::spawn_blocking({
                            let provider = config.history_provider.clone();
                            let data_root = config.data_root.clone();
                            move || {
                                load_option_eod_bars(
                                    &data_root,
                                    &ticker,
                                    current_date,
                                    provider.as_ref(),
                                )
                            }
                        })
                        .await
                        .unwrap_or_default();
                        if !bars.is_empty() {
                            build_option_chain_from_eod_bars(canonical, spot, utc_time, &bars)
                        } else {
                            OptionChain::new(canonical.clone(), spot)
                        }
                    };
                    chains_for_day.push((canonical.permtick.clone(), chain));
                }

                let mut alg = adapter.inner.lock().unwrap();
                for (permtick, chain) in chains_for_day {
                    alg.option_chains.insert(permtick, chain);
                }
            }

            let chains_snapshot: Vec<(String, OptionChain)> = {
                let alg = adapter.inner.lock().unwrap();
                alg.option_chains
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            sync_option_holdings_to_chain_prices(&adapter, &portfolio, &chains_snapshot);

            // ── Custom data fetch (daily) ─────────────────────────────────
            let custom_subs: Vec<CustomDataSubscription> = {
                adapter
                    .inner
                    .lock()
                    .unwrap()
                    .custom_data_subscriptions
                    .clone()
            };
            let mut custom_data_for_day: HashMap<String, Vec<CustomDataPoint>> = HashMap::new();
            for sub in &custom_subs {
                let key = sub.ticker.to_uppercase();
                if let Some(by_date) = custom_history.get(&key) {
                    // Full-history source: look up from preloaded in-memory map.
                    if let Some(pts) = by_date.get(&current_date) {
                        custom_data_for_day.insert(sub.ticker.clone(), pts.clone());
                    }
                } else {
                    // Date-keyed source: per-day HTTP fetch with per-day Parquet cache.
                    let source = config
                        .custom_data_sources
                        .iter()
                        .find(|s| s.name() == sub.source_type)
                        .cloned();
                    let data_root = config.data_root.clone();
                    let source_type = sub.source_type.clone();
                    let ticker = sub.ticker.clone();
                    let cfg = sub.config.clone();
                    let dynamic_query = sub.dynamic_query.clone();
                    let points = load_custom_data_points_for_subscription(
                        data_root,
                        source_type,
                        ticker,
                        current_date,
                        source,
                        cfg,
                        dynamic_query,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "failed to load custom data for {}/{} {}",
                            sub.source_type, sub.ticker, current_date
                        )
                    })?;
                    if !points.is_empty() {
                        custom_data_for_day.insert(sub.ticker.clone(), points);
                    }
                }
            }

            // ── Map file: check for ticker renames and delistings ────────────
            for sub in &subscriptions {
                let sid = sub.symbol.id.sid;
                let Some(rows) = map_file_map.get(&sid) else {
                    continue;
                };

                // Check for rename (ticker change between yesterday and today)
                let today_ticker = ticker_at_date(rows, current_date);
                let yesterday_ticker =
                    ticker_at_date(rows, current_date - chrono::Duration::days(1));
                if let (Some(old), Some(new)) = (yesterday_ticker, today_ticker) {
                    if old != new {
                        let ev = SymbolChangedEvent::new(
                            sub.symbol.clone(),
                            utc_time,
                            old.to_string(),
                            new.to_string(),
                        );
                        slice.add_symbol_changed(ev);
                        info!("Symbol rename: {} → {} on {}", old, new, current_date);
                    }
                }

                // Check for delisting
                if let Some(delist_date) = delisting_date(rows) {
                    let last_price = portfolio.get_holding(&sub.symbol).last_price;
                    if current_date == delist_date {
                        // Warning: last day of trading
                        slice.add_delisting(Delisting::new(
                            sub.symbol.clone(),
                            utc_time,
                            last_price,
                            DelistingType::Warning,
                        ));
                        // Auto-liquidate: place market order to close position
                        if portfolio.is_invested(&sub.symbol) {
                            let holding = portfolio.get_holding(&sub.symbol);
                            let qty = -holding.quantity;
                            adapter.inner.lock().unwrap().market_order(&sub.symbol, qty);
                        }
                    } else if current_date == delist_date + chrono::Duration::days(1) {
                        slice.add_delisting(Delisting::new(
                            sub.symbol.clone(),
                            utc_time,
                            last_price,
                            DelistingType::Delisted,
                        ));
                    }
                }
            }

            Python::attach(|py| {
                slice_proxy.update_option_chains(py, &chains_snapshot);
                slice_proxy.update_quote_bars(py, &HashMap::new());
                slice_proxy.update_ticks(py, &HashMap::new());
                slice_proxy.update_custom_data(py, &custom_data_for_day);
                adapter.on_data_proxy(py, &slice_proxy, &slice);

                // Fire on_delistings if the slice contains delisting events.
                if !slice.delistings.is_empty() {
                    let delistings = slice_proxy.delistings_cell.clone_ref(py);
                    adapter.on_delistings(py, delistings);
                }

                // Fire on_symbol_changed_events if the slice contains rename events.
                if !slice.symbol_changed_events.is_empty() {
                    let sce = slice_proxy.symbol_changed_events_cell.clone_ref(py);
                    adapter.on_symbol_changed_events(py, sce);
                }
            });

            // ── Algorithm Framework pipeline ──────────────────────────────
            // Run alpha → PCM → risk → execution after on_data, outside GIL.
            // Only fires when at least one alpha model has been registered.
            {
                let order_requests =
                    run_framework_pipeline(&adapter.framework, &adapter.inner, &slice);
                if !order_requests.is_empty() {
                    let mut alg = adapter.inner.lock().unwrap();
                    for req in order_requests {
                        use lean_execution::ExecutionOrderType;
                        match req.order_type {
                            ExecutionOrderType::Market => {
                                alg.market_order(&req.symbol, req.quantity);
                            }
                            ExecutionOrderType::Limit => {
                                if let Some(lp) = req.limit_price {
                                    alg.limit_order(&req.symbol, req.quantity, lp);
                                }
                            }
                            ExecutionOrderType::MarketOnOpen => {
                                alg.market_on_open_order(&req.symbol, req.quantity);
                            }
                            ExecutionOrderType::MarketOnClose => {
                                alg.market_on_close_order(&req.symbol, req.quantity);
                            }
                        }
                    }
                }
            }

            adapter.on_end_of_day(None);

            process_option_expirations(&mut adapter, current_date, &split_ratios_today);

            let day_equity = portfolio.total_portfolio_value();
            equity_curve.push(day_equity);
            daily_dates.push(current_date.to_string());
            if let Some(writer) = &mut live_writer {
                writer.record_progress(
                    current_date,
                    trading_days,
                    day_equity,
                    all_order_events.len(),
                    completed_trades.len(),
                );
            }

            current_date += chrono::Duration::days(1);
        }
    }

    adapter.on_end_of_algorithm();

    let sim_elapsed = sim_start.elapsed();
    let pts_per_sec = if sim_elapsed.as_secs_f64() > 0.0 {
        trading_days as f64 / sim_elapsed.as_secs_f64()
    } else {
        f64::INFINITY
    };
    println!(
        "Simulation: {:.0} trading days in {:.0}ms ({:.0} days/sec)",
        trading_days,
        sim_elapsed.as_millis(),
        pts_per_sec
    );

    let final_value = {
        use rust_decimal::prelude::ToPrimitive;
        portfolio.total_portfolio_value().to_f64().unwrap_or(0.0)
    };
    let total_return = if starting_cash > 0.0 {
        (final_value - starting_cash) / starting_cash
    } else {
        0.0
    };

    let starting_cash_dec = Decimal::from_f64(starting_cash).unwrap_or(Decimal::ONE);
    // Use benchmark_curve only when it aligns with the equity curve length;
    // mismatches (e.g. benchmark had no data on some days) fall back to empty.
    let benchmark_slice: &[Decimal] = if benchmark_curve.len() == equity_curve.len() {
        &benchmark_curve
    } else {
        &[]
    };
    let statistics = PortfolioStatistics::compute(
        &equity_curve,
        benchmark_slice,
        &completed_trades,
        trading_days,
        starting_cash_dec,
        Decimal::from_f64(0.05 / 252.0).unwrap_or(Decimal::ZERO), // ~5% annual risk-free
    );

    let equity_curve_f64: Vec<f64> = equity_curve
        .iter()
        .map(|v| {
            use rust_decimal::prelude::ToPrimitive;
            v.to_f64().unwrap_or(0.0)
        })
        .collect();
    if let Some(writer) = &live_writer {
        writer.mark_completed(
            trading_days,
            portfolio.total_portfolio_value(),
            all_order_events.len(),
            completed_trades.len(),
        );
    }

    // Collect charts from the strategy after the backtest completes.
    let charts = adapter.charts.lock().map(|c| c.clone()).unwrap_or_default();

    Ok(BacktestResult {
        trading_days,
        final_value,
        total_return,
        starting_cash,
        start_date,
        end_date,
        equity_curve: equity_curve_f64,
        daily_dates,
        statistics,
        charts,
        order_events: all_order_events,
        succeeded_data_requests,
        failed_data_requests,
        backtest_id,
        benchmark_symbol: effective_benchmark_ticker,
    })
}

/// Before the backtest loop, fetch any symbols that don't have local data.
///
/// For daily/hourly (single-file per ticker): checks once and fetches the
/// entire date range in one API call.
///
/// For minute/second (date-partitioned): checks for the first date; if
/// missing, fetches the full range and the provider splits by date on write.
async fn pre_fetch_all(
    provider: Arc<dyn IHistoricalDataProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<()> {
    if subscriptions.is_empty() {
        return Ok(());
    }

    info!(
        "Prefetching {} subscriptions with parallelism {} ({} → {})",
        subscriptions.len(),
        SUBSCRIPTION_PREFETCH_CONCURRENCY.min(subscriptions.len()),
        start,
        end
    );

    for chunk in subscriptions.chunks(SUBSCRIPTION_PREFETCH_CONCURRENCY) {
        let mut tasks = tokio::task::JoinSet::new();
        for sub in chunk {
            let provider = provider.clone();
            let factor_provider = factor_provider.clone();
            let sub = sub.clone();
            let resolver = resolver.clone();
            tasks.spawn(async move {
                pre_fetch_subscription(provider, factor_provider, sub, start, end, resolver).await
            });
        }

        while let Some(result) = tasks.join_next().await {
            result.map_err(|e| anyhow::anyhow!("prefetch task failed: {}", e))??;
        }
    }

    Ok(())
}

async fn pre_fetch_subscription(
    provider: Arc<dyn IHistoricalDataProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    sub: Arc<SubscriptionDataConfig>,
    start: NaiveDate,
    end: NaiveDate,
    resolver: PathResolver,
) -> Result<()> {
    let data_path = if sub.resolution == Resolution::Tick {
        resolver.tick(&sub.symbol, start).to_path()
    } else if sub.tick_type == TickType::Quote {
        resolver
            .quote_bar(&sub.symbol, sub.resolution, start)
            .to_path()
    } else {
        resolver
            .trade_bar(&sub.symbol, sub.resolution, start)
            .to_path()
    };

    let ticker = sub.symbol.permtick.to_lowercase();
    let market = sub.symbol.market().as_str().to_lowercase();
    let sec = format!("{}", sub.symbol.security_type()).to_lowercase();
    let factor_path = resolver
        .data_root
        .join(&sec)
        .join(&market)
        .join("factor_files")
        .join(format!("{ticker}.parquet"));

    // A factor file is valid if it exists, is non-empty, AND the sentinel row
    // (row 0, dated when the file was last generated) covers the backtest end.
    let factor_valid = factor_path.exists() && {
        let r = ParquetReader::new();
        r.read_factor_file(&factor_path).is_ok_and(|rows| {
            if rows.is_empty() {
                return false;
            }
            rows[0].date >= end
        })
    };

    let data_covers_range = local_data_covers_range(&sub, start, end, &resolver, &data_path).await;

    if data_covers_range && factor_valid {
        return Ok(());
    }

    // Clip start to the provider's earliest supported date (e.g. ThetaData
    // STANDARD only has data from 2018-01-01; requesting earlier causes 403).
    let effective_start = match provider.earliest_date() {
        Some(earliest) if start < earliest => {
            warn!(
                "Provider earliest date is {}; clipping backtest start from {} for {}",
                earliest, start, sub.symbol.value
            );
            earliest
        }
        _ => start,
    };

    let start_dt = date_to_datetime(effective_start, 0, 0, 0);
    let end_dt = date_to_datetime(end, 23, 59, 59);

    if !data_covers_range
        && (sub.resolution.is_high_resolution() || sub.tick_type == TickType::Quote)
    {
        let rows_downloaded = pre_fetch_missing_high_resolution_days(
            provider.clone(),
            factor_provider.clone(),
            &sub,
            effective_start,
            end,
            &resolver,
        )
        .await?;
        if rows_downloaded > 0 {
            info!(
                "Downloaded {} high-resolution rows for {} and cached to disk",
                rows_downloaded, sub.symbol.value
            );
        } else {
            warn!(
                "Historical provider returned 0 high-resolution rows for {} ({} → {}); no cache file was written",
                sub.symbol.value, effective_start, end
            );
        }
    } else if !data_covers_range {
        // LEAN's DownloaderDataProvider downloads daily/hourly as a whole
        // single-file data source. Do the same by using the provider's earliest
        // supported date when known, so a later narrower request cannot replace
        // a wider cached file with only the requested slice.
        let single_file_start = provider.earliest_date().unwrap_or(effective_start);
        let single_file_start_dt = date_to_datetime(single_file_start, 0, 0, 0);
        info!(
            "Local data missing or incomplete for {} — fetching single-file range from provider ({} → {})",
            sub.symbol.value, single_file_start, end
        );
        let bars = provider
            .get_trade_bars(
                sub.symbol.clone(),
                sub.resolution,
                single_file_start_dt,
                end_dt,
            )
            .await
            .map_err(|e| {
                anyhow::anyhow!("historical provider failed for {}: {}", sub.symbol.value, e)
            })?;
        info!(
            "Downloaded {} bars for {} and cached to disk",
            bars.len(),
            sub.symbol.value
        );
    }

    let factor_needs_update = !factor_path.exists() || {
        let r = ParquetReader::new();
        r.read_factor_file(&factor_path)
            .is_ok_and(|rows| rows.is_empty() || rows[0].date < end)
    };
    if factor_needs_update {
        if let Some(ref fp) = factor_provider {
            info!(
                "Factor file missing or stale for {} — requesting from provider",
                sub.symbol.value
            );
            let fp = Arc::clone(fp);
            let request = lean_data_providers::HistoryRequest {
                symbol: sub.symbol.clone(),
                resolution: lean_core::Resolution::Daily,
                start: start_dt,
                end: end_dt,
                data_type: lean_data_providers::DataType::FactorFile,
            };
            match tokio::task::spawn_blocking(move || fp.get_history(&request)).await {
                Ok(Ok(_)) => info!("Factor file generated for {}", sub.symbol.value),
                Ok(Err(e)) => warn!(
                    "Factor file generation failed for {}: {e}",
                    sub.symbol.value
                ),
                Err(e) => warn!("Factor file task panicked for {}: {e}", sub.symbol.value),
            }
        }
    }

    Ok(())
}

async fn pre_fetch_missing_high_resolution_days(
    provider: Arc<dyn IHistoricalDataProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<usize> {
    let dates = expected_market_dates(&sub.symbol, start, end);
    let mut missing = Vec::new();
    for date in dates {
        let path = if sub.resolution == Resolution::Tick {
            resolver.tick(&sub.symbol, date).to_path()
        } else if sub.tick_type == TickType::Quote {
            resolver
                .quote_bar(&sub.symbol, sub.resolution, date)
                .to_path()
        } else {
            resolver
                .trade_bar(&sub.symbol, sub.resolution, date)
                .to_path()
        };
        if !path.exists() {
            missing.push(date);
        }
    }

    if missing.is_empty() {
        return Ok(0);
    }

    info!(
        "Local data missing or incomplete for {} — fetching {} missing {} files date-by-date with parallelism {} ({} → {})",
        sub.symbol.value,
        missing.len(),
        sub.resolution,
        HIGH_RESOLUTION_PREFETCH_CONCURRENCY.min(missing.len()),
        missing.first().unwrap(),
        missing.last().unwrap()
    );

    let mut rows_downloaded = 0usize;
    let mut completed = 0usize;
    for chunk in missing.chunks(HIGH_RESOLUTION_PREFETCH_CONCURRENCY) {
        let mut tasks = tokio::task::JoinSet::new();

        for date in chunk.iter().copied() {
            let provider = provider.clone();
            let factor_provider = factor_provider.clone();
            let symbol = sub.symbol.clone();
            let resolution = sub.resolution;
            let tick_type = sub.tick_type;
            let symbol_value = sub.symbol.value.clone();

            tasks.spawn(async move {
                let start_dt = date_to_datetime(date, 0, 0, 0);
                let end_dt = date_to_datetime(date, 23, 59, 59);

                if resolution == Resolution::Tick || tick_type == TickType::Quote {
                    let Some(fp) = factor_provider else {
                        warn!(
                            "No sync history provider configured for {} {} download",
                            symbol_value, resolution
                        );
                        return Ok((date, 0usize));
                    };
                    let request = lean_data_providers::HistoryRequest {
                        symbol,
                        resolution,
                        start: start_dt,
                        end: end_dt,
                        data_type: if resolution == Resolution::Tick {
                            lean_data_providers::DataType::Tick
                        } else {
                            lean_data_providers::DataType::QuoteBar
                        },
                    };
                    let rows = if resolution == Resolution::Tick {
                        match tokio::task::spawn_blocking(move || fp.get_ticks(&request)).await {
                            Ok(Ok(rows)) => rows.len(),
                            Ok(Err(e)) => {
                                return Err(anyhow::anyhow!(
                                    "historical provider failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                ));
                            }
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "historical provider task failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                ));
                            }
                        }
                    } else {
                        match tokio::task::spawn_blocking(move || fp.get_quote_bars(&request)).await
                        {
                            Ok(Ok(rows)) => rows.len(),
                            Ok(Err(e)) => {
                                return Err(anyhow::anyhow!(
                                    "historical provider failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                ));
                            }
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "historical provider task failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                ));
                            }
                        }
                    };
                    Ok((date, rows))
                } else {
                    let bars = provider
                        .get_trade_bars(symbol, resolution, start_dt, end_dt)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "historical provider failed for {} {}: {}",
                                symbol_value,
                                date,
                                e
                            )
                        })?;
                    Ok((date, bars.len()))
                }
            });
        }

        while let Some(result) = tasks.join_next().await {
            let (date, rows) =
                result.map_err(|e| anyhow::anyhow!("historical provider task failed: {}", e))??;
            completed += 1;
            rows_downloaded += rows;

            if completed == 1 || completed.is_multiple_of(50) || completed == missing.len() {
                info!(
                    "Fetched {} {} missing day {}/{} ({})",
                    sub.symbol.value,
                    sub.resolution,
                    completed,
                    missing.len(),
                    date
                );
            }
        }
    }

    Ok(rows_downloaded)
}

async fn local_data_covers_range(
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
    start_path: &std::path::Path,
) -> bool {
    let expected_dates = expected_market_dates(&sub.symbol, start, end);
    if expected_dates.is_empty() {
        return true;
    }

    if sub.resolution.is_high_resolution() || sub.tick_type == TickType::Quote {
        for current in &expected_dates {
            let path = if sub.resolution == Resolution::Tick {
                resolver.tick(&sub.symbol, *current).to_path()
            } else if sub.tick_type == TickType::Quote {
                resolver
                    .quote_bar(&sub.symbol, sub.resolution, *current)
                    .to_path()
            } else {
                resolver
                    .trade_bar(&sub.symbol, sub.resolution, *current)
                    .to_path()
            };
            if !path.exists() {
                return false;
            }
        }
        return true;
    }

    if !start_path.exists() {
        return false;
    }

    let reader = ParquetReader::new();
    let params = QueryParams::new().with_time_range(
        date_to_datetime(start, 0, 0, 0),
        date_to_datetime(end, 23, 59, 59),
    );
    let bars = reader
        .read_trade_bars(&[start_path.to_path_buf()], sub.symbol.clone(), &params)
        .await
        .unwrap_or_default();

    let available: HashSet<NaiveDate> = bars.iter().map(|bar| bar.time.date_utc()).collect();
    expected_dates.iter().all(|date| available.contains(date))
}

fn expected_market_dates(symbol: &Symbol, start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    let mut current = start;
    while current <= end {
        if is_expected_market_date(symbol, current) {
            dates.push(current);
        }
        current += chrono::Duration::days(1);
    }
    dates
}

fn is_expected_market_date(symbol: &Symbol, date: NaiveDate) -> bool {
    match symbol.security_type() {
        SecurityType::Equity | SecurityType::Option | SecurityType::IndexOption => {
            let hours = ExchangeHours::us_equity();
            let dow = date.weekday().num_days_from_sunday() as usize;
            hours.schedule[dow].is_open() && !hours.holidays.contains(&date)
        }
        SecurityType::Crypto => true,
        _ => !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun),
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap()))
}

fn day_key(date: NaiveDate) -> i64 {
    date.signed_duration_since(NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
        .num_days()
}

#[derive(Debug, Clone)]
struct OptionChainRuntime {
    permtick: String,
    chain: OptionChain,
    trade_updates: HashMap<i64, Vec<TradeBar>>,
    quote_updates: HashMap<i64, Vec<QuoteBar>>,
    tick_updates: HashMap<i64, Vec<Tick>>,
}

impl OptionChainRuntime {
    fn apply_timestamp(&mut self, valuation_time: DateTime, spot: Decimal) {
        self.chain.underlying_price = spot;
        for contract in self.chain.contracts.values_mut() {
            contract.data.underlying_last_price = spot;
        }

        if let Some(bars) = self.trade_updates.get(&valuation_time.0) {
            for bar in bars {
                apply_option_trade_bar(&mut self.chain, bar, spot);
            }
        }
        if let Some(bars) = self.quote_updates.get(&valuation_time.0) {
            for bar in bars {
                apply_option_quote_bar(&mut self.chain, bar, spot);
            }
        }
        if let Some(ticks) = self.tick_updates.get(&valuation_time.0) {
            for tick in ticks {
                apply_option_tick(&mut self.chain, tick, spot);
            }
        }

        reprice_option_chain(&mut self.chain, valuation_time);
    }

    fn timestamps(&self) -> Vec<i64> {
        let mut out = std::collections::BTreeSet::new();
        out.extend(self.trade_updates.keys().copied());
        out.extend(self.quote_updates.keys().copied());
        out.extend(self.tick_updates.keys().copied());
        out.into_iter().collect()
    }
}

fn load_option_chain_runtime(
    data_root: &Path,
    ticker: &str,
    canonical: &Symbol,
    resolution: Resolution,
    date: NaiveDate,
    spot: Decimal,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> OptionChainRuntime {
    let universe_rows = load_option_universe_rows(data_root, ticker, date, provider);
    let chain = build_option_chain_from_universe_rows(canonical, spot, &universe_rows);

    let (trade_updates, quote_updates, tick_updates) = match resolution {
        Resolution::Minute => (
            group_trade_bars_by_time(load_option_trade_bars(
                data_root, ticker, resolution, date, provider,
            )),
            group_quote_bars_by_time(load_option_quote_bars(
                data_root, ticker, resolution, date, provider,
            )),
            HashMap::new(),
        ),
        Resolution::Tick => (
            HashMap::new(),
            HashMap::new(),
            group_ticks_by_time(load_option_ticks(data_root, ticker, date, provider)),
        ),
        _ => (HashMap::new(), HashMap::new(), HashMap::new()),
    };

    OptionChainRuntime {
        permtick: canonical.permtick.clone(),
        chain,
        trade_updates,
        quote_updates,
        tick_updates,
    }
}

fn load_option_universe_rows(
    data_root: &Path,
    ticker: &str,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<OptionUniverseRow> {
    let result = if let Some(provider) = provider {
        provider.get_option_universe(ticker, date)
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root).get_option_universe(ticker, date)
    };

    result.unwrap_or_else(|e| {
        warn!("option universe fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

fn load_option_trade_bars(
    data_root: &Path,
    ticker: &str,
    resolution: Resolution,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<TradeBar> {
    let result = if let Some(provider) = provider {
        provider.get_option_trade_bars(ticker, resolution, date)
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_trade_bars(ticker, resolution, date)
    };

    result.unwrap_or_else(|e| {
        warn!("option trade-bar fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

fn load_option_quote_bars(
    data_root: &Path,
    ticker: &str,
    resolution: Resolution,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<QuoteBar> {
    let result = if let Some(provider) = provider {
        provider.get_option_quote_bars(ticker, resolution, date)
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_quote_bars(ticker, resolution, date)
    };

    result.unwrap_or_else(|e| {
        warn!("option quote-bar fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

fn load_option_ticks(
    data_root: &Path,
    ticker: &str,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<Tick> {
    let result = if let Some(provider) = provider {
        provider.get_option_ticks(ticker, date)
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root).get_option_ticks(ticker, date)
    };

    result.unwrap_or_else(|e| {
        warn!("option tick fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

fn build_option_chain_from_universe_rows(
    canonical_sym: &Symbol,
    spot: Decimal,
    rows: &[OptionUniverseRow],
) -> OptionChain {
    let mut chain = OptionChain::new(canonical_sym.clone(), spot);
    let underlying_sym = canonical_sym
        .underlying
        .as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| {
            Symbol::create_equity(
                canonical_sym.permtick.trim_start_matches('?'),
                &Market::usa(),
            )
        });

    for row in rows {
        let right = match row.right.to_ascii_uppercase().as_str() {
            "C" | "CALL" => OptionRight::Call,
            "P" | "PUT" => OptionRight::Put,
            _ => continue,
        };
        let sym = Symbol::create_option_osi(
            underlying_sym.clone(),
            row.strike,
            row.expiration,
            right,
            OptionStyle::American,
            &Market::usa(),
        );
        let mut contract = OptionContract::new(sym);
        contract.data.underlying_last_price = spot;
        chain.add_contract(contract);
    }

    chain
}

fn reprice_option_chain(chain: &mut OptionChain, valuation_time: DateTime) {
    let model = BlackScholesPriceModel;
    for contract in chain.contracts.values_mut() {
        evaluate_contract_with_market_iv(&model, contract, valuation_time, 0.0, 0.0);
    }
}

fn apply_option_trade_bar(chain: &mut OptionChain, bar: &TradeBar, spot: Decimal) {
    use rust_decimal::prelude::ToPrimitive;

    if let Some(contract) = chain.contracts.get_mut(&bar.symbol) {
        contract.data.underlying_last_price = spot;
        contract.data.last_price = bar.close;
        contract.data.volume = bar.volume.to_i64().unwrap_or(contract.data.volume);
    }
}

fn apply_option_quote_bar(chain: &mut OptionChain, bar: &QuoteBar, spot: Decimal) {
    use rust_decimal_macros::dec;

    if let Some(contract) = chain.contracts.get_mut(&bar.symbol) {
        contract.data.underlying_last_price = spot;
        contract.data.bid_price = bar.bid.as_ref().map(|b| b.close).unwrap_or(Decimal::ZERO);
        contract.data.ask_price = bar.ask.as_ref().map(|a| a.close).unwrap_or(Decimal::ZERO);
        contract.data.bid_size = bar
            .last_bid_size
            .round()
            .to_i64()
            .unwrap_or(contract.data.bid_size);
        contract.data.ask_size = bar
            .last_ask_size
            .round()
            .to_i64()
            .unwrap_or(contract.data.ask_size);
        if contract.data.last_price <= Decimal::ZERO
            && contract.data.bid_price > Decimal::ZERO
            && contract.data.ask_price > Decimal::ZERO
        {
            contract.data.last_price =
                (contract.data.bid_price + contract.data.ask_price) / dec!(2);
        }
    }
}

fn apply_option_tick(chain: &mut OptionChain, tick: &Tick, spot: Decimal) {
    use rust_decimal::prelude::ToPrimitive;

    if let Some(contract) = chain.contracts.get_mut(&tick.symbol) {
        contract.data.underlying_last_price = spot;
        match tick.tick_type {
            TickType::Trade => {
                contract.data.last_price = tick.value;
                contract.data.volume = tick
                    .quantity
                    .round()
                    .to_i64()
                    .unwrap_or(contract.data.volume);
            }
            TickType::Quote => {
                contract.data.bid_price = tick.bid_price;
                contract.data.ask_price = tick.ask_price;
                contract.data.bid_size = tick
                    .bid_size
                    .round()
                    .to_i64()
                    .unwrap_or(contract.data.bid_size);
                contract.data.ask_size = tick
                    .ask_size
                    .round()
                    .to_i64()
                    .unwrap_or(contract.data.ask_size);
                if contract.data.last_price <= Decimal::ZERO && tick.value > Decimal::ZERO {
                    contract.data.last_price = tick.value;
                }
            }
            TickType::OpenInterest => {
                contract.data.open_interest = tick.value;
            }
        }
    }
}

fn group_trade_bars_by_time(bars: Vec<TradeBar>) -> HashMap<i64, Vec<TradeBar>> {
    let mut by_time: HashMap<i64, Vec<TradeBar>> = HashMap::new();
    for bar in bars {
        by_time.entry(bar.time.0).or_default().push(bar);
    }
    by_time
}

fn group_quote_bars_by_time(bars: Vec<QuoteBar>) -> HashMap<i64, Vec<QuoteBar>> {
    let mut by_time: HashMap<i64, Vec<QuoteBar>> = HashMap::new();
    for bar in bars {
        by_time.entry(bar.time.0).or_default().push(bar);
    }
    by_time
}

fn group_ticks_by_time(ticks: Vec<Tick>) -> HashMap<i64, Vec<Tick>> {
    let mut by_time: HashMap<i64, Vec<Tick>> = HashMap::new();
    for tick in ticks {
        by_time.entry(tick.time.0).or_default().push(tick);
    }
    by_time
}

fn quote_bar_mid_ohlc(bar: &QuoteBar) -> Option<(Decimal, Decimal, Decimal, Decimal)> {
    let open = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.open + ask.open) / Decimal::from(2),
        (Some(bid), None) => bid.open,
        (None, Some(ask)) => ask.open,
        (None, None) => return None,
    };
    let high = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.high + ask.high) / Decimal::from(2),
        (Some(bid), None) => bid.high,
        (None, Some(ask)) => ask.high,
        (None, None) => return None,
    };
    let low = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.low + ask.low) / Decimal::from(2),
        (Some(bid), None) => bid.low,
        (None, Some(ask)) => ask.low,
        (None, None) => return None,
    };
    let close = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.close + ask.close) / Decimal::from(2),
        (Some(bid), None) => bid.close,
        (None, Some(ask)) => ask.close,
        (None, None) => return None,
    };
    Some((open, high, low, close))
}

fn synthesize_trade_bar_from_quote_bar(bar: &QuoteBar) -> Option<TradeBar> {
    let (open, high, low, close) = quote_bar_mid_ohlc(bar)?;
    Some(TradeBar::new(
        bar.symbol.clone(),
        bar.time,
        bar.period,
        TradeBarData::new(open, high, low, close, Decimal::ZERO),
    ))
}

fn synthesize_trade_bar_from_ticks(
    symbol: &Symbol,
    time: DateTime,
    ticks: &[Tick],
) -> Option<TradeBar> {
    if ticks.is_empty() {
        return None;
    }

    let trade_prices: Vec<Decimal> = ticks
        .iter()
        .filter(|tick| tick.tick_type == TickType::Trade && tick.value > Decimal::ZERO)
        .map(|tick| tick.value)
        .collect();

    let volume = ticks
        .iter()
        .filter(|tick| tick.tick_type == TickType::Trade)
        .fold(Decimal::ZERO, |acc, tick| acc + tick.quantity);

    let prices = if !trade_prices.is_empty() {
        trade_prices
    } else {
        ticks
            .iter()
            .filter_map(|tick| match tick.tick_type {
                TickType::Trade if tick.value > Decimal::ZERO => Some(tick.value),
                TickType::Quote if tick.value > Decimal::ZERO => Some(tick.value),
                _ => None,
            })
            .collect()
    };

    let open = *prices.first()?;
    let close = *prices.last()?;
    let high = prices.iter().copied().max()?;
    let low = prices.iter().copied().min()?;

    Some(TradeBar::new(
        symbol.clone(),
        time,
        TimeSpan::ZERO,
        TradeBarData::new(open, high, low, close, volume),
    ))
}

/// Process option expirations for `current_date`.
///
/// Scans all open option positions for contracts expiring today, computes
/// intrinsic value, and handles exercise (long) or assignment (short).
///
/// `split_ratios` — map of underlying SID → forward split ratio for splits that
/// occurred on `current_date`.  When an option underlying had a split today, the
/// option's strike is divided by the ratio before the ITM/OTM comparison so that
/// pre-split strikes are evaluated against the post-split spot price correctly.
fn process_option_expirations(
    adapter: &mut PyAlgorithmAdapter,
    current_date: NaiveDate,
    split_ratios: &HashMap<u64, f64>,
) {
    // Collect expiring positions — we need to drop the lock before calling market_order.
    let expiring: Vec<lean_algorithm::qc_algorithm::OpenOptionPosition> = adapter
        .inner
        .lock()
        .unwrap()
        .get_option_positions()
        .into_iter()
        .filter(|pos| pos.expiry == current_date)
        .collect();

    if expiring.is_empty() {
        return;
    }

    for pos in expiring {
        // Get the spot price for the underlying.
        let spot = {
            let alg = adapter.inner.lock().unwrap();
            // Try to find the underlying security by permtick.
            let underlying_ticker = pos
                .symbol
                .underlying
                .as_ref()
                .map(|u| u.permtick.clone())
                .unwrap_or_default();
            let found: Option<Decimal> = alg
                .securities
                .all()
                .find(|s| s.symbol.permtick.eq_ignore_ascii_case(&underlying_ticker))
                .map(|s| s.current_price());
            found.unwrap_or(pos.strike) // Conservative fallback: use strike
        };

        // If the option's underlying had a forward split today, options written in
        // the pre-split era carry a strike in pre-split price terms while `spot` is
        // in post-split terms.  Divide the strike by the split ratio so the
        // comparison is in a consistent price space.
        let effective_strike = if let Some(underlying) = &pos.symbol.underlying {
            if let Some(&ratio) = split_ratios.get(&underlying.id.sid) {
                let ratio_dec = Decimal::from_f64(ratio).unwrap_or(Decimal::ONE);
                if ratio_dec > Decimal::ZERO {
                    let adj = pos.strike / ratio_dec;
                    info!(
                        "Split-adjusted strike for {}: {:.4} → {:.4} (÷{:.4})",
                        pos.symbol.value, pos.strike, adj, ratio
                    );
                    adj
                } else {
                    pos.strike
                }
            } else {
                pos.strike
            }
        } else {
            pos.strike
        };

        let intrinsic = intrinsic_value(spot, effective_strike, pos.right);
        let exercised = intrinsic >= rust_decimal_macros::dec!(0.01);

        if exercised && pos.quantity > Decimal::ZERO {
            // Long position: auto-exercise
            let contracts = pos.quantity;
            let underlying_sym = pos
                .symbol
                .underlying
                .as_ref()
                .map(|u| *u.clone())
                .unwrap_or_else(|| pos.symbol.clone());

            // Shares from exercise: get_exercise_quantity uses LEAN sign convention.
            // For a long call: caller buys 100*qty shares, pays strike*100*qty.
            // For a long put: caller sells 100*qty shares, receives strike*100*qty.
            let exercise_shares = get_exercise_quantity(contracts, pos.right, 100);
            {
                let alg = adapter.inner.lock().unwrap();
                alg.portfolio.settle_fill_without_cash(
                    &pos.symbol,
                    Decimal::ZERO,
                    -contracts,
                    Decimal::from(pos.contract_unit_of_trade),
                );
                alg.portfolio.apply_exercise_with_market_price(
                    &underlying_sym,
                    effective_strike,
                    exercise_shares,
                    spot,
                );
            }

            info!(
                "Option exercised: {} x{} K={} (effective={}) expiry={}",
                pos.symbol.value, contracts, pos.strike, effective_strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, contracts, true);
        } else if exercised && pos.quantity < Decimal::ZERO {
            // Short position: assignment
            let contracts = pos.quantity.abs();
            let underlying_sym = pos
                .symbol
                .underlying
                .as_ref()
                .map(|u| *u.clone())
                .unwrap_or_else(|| pos.symbol.clone());

            {
                let alg = adapter.inner.lock().unwrap();
                alg.portfolio.settle_fill_without_cash(
                    &pos.symbol,
                    Decimal::ZERO,
                    contracts,
                    Decimal::from(pos.contract_unit_of_trade),
                );
                let shares = Decimal::from(100) * contracts;
                let exercise_qty = match pos.right {
                    OptionRight::Put => shares,   // buy stock
                    OptionRight::Call => -shares, // sell (or short) stock
                };
                alg.portfolio.apply_exercise_with_market_price(
                    &underlying_sym,
                    effective_strike,
                    exercise_qty,
                    spot,
                );
            }

            info!(
                "Option assigned: {} x{} K={} (effective={}) expiry={}",
                pos.symbol.value, pos.quantity, pos.strike, effective_strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, contracts, true);
        } else {
            // Expired worthless — premium already booked at trade open.
            let entry_price = pos.entry_price;
            adapter
                .inner
                .lock()
                .unwrap()
                .portfolio
                .settle_fill_without_cash(
                    &pos.symbol,
                    Decimal::ZERO,
                    -pos.quantity,
                    Decimal::from(pos.contract_unit_of_trade),
                );
            info!(
                "Option expired worthless: {} x{} K={} expiry={}",
                pos.symbol.value, pos.quantity, pos.strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            // LEAN fires on_order_event (not on_assignment_order_event) for OTM expiry.
            adapter.on_otm_expiry(contract, pos.quantity.abs(), spot, entry_price);
        }
    }
}

fn sync_option_holdings_to_chain_prices(
    adapter: &PyAlgorithmAdapter,
    portfolio: &Arc<lean_algorithm::portfolio::SecurityPortfolioManager>,
    chains: &[(String, OptionChain)],
) {
    let holdings = portfolio.all_holdings();
    if holdings.is_empty() {
        return;
    }

    let chain_map: HashMap<&str, &OptionChain> = chains
        .iter()
        .map(|(permtick, chain)| (permtick.as_str(), chain))
        .collect();

    for holding in holdings {
        if !holding.is_invested() || holding.symbol.option_symbol_id().is_none() {
            continue;
        }
        let Some(underlying) = holding.symbol.underlying.as_ref() else {
            continue;
        };
        let canonical = format!("?{}", underlying.permtick);
        let Some(chain) = chain_map.get(canonical.as_str()) else {
            continue;
        };
        let Some((_, contract)) = chain
            .contracts
            .iter()
            .find(|(symbol, _)| symbol.id.sid == holding.symbol.id.sid)
        else {
            continue;
        };

        let price = contract.mid_price();
        if price <= Decimal::ZERO {
            continue;
        }

        portfolio.update_prices(&holding.symbol, price);
        adapter
            .inner
            .lock()
            .unwrap()
            .securities
            .update_price(&holding.symbol, price);
    }
}

/// Build a real option chain from ThetaData EOD rows for a single trading day.
///
/// Build an option chain directly from typed `OptionEodBar` rows.
///
/// Avoids the intermediate `V3OptionEod` representation — no string→date or
/// f64→Decimal round-trips.  Contracts expiring on or before `today` are skipped.
fn build_option_chain_from_eod_bars(
    canonical_sym: &Symbol,
    spot: Decimal,
    valuation_time: DateTime,
    bars: &[OptionEodBar],
) -> OptionChain {
    let today = valuation_time.date_utc();
    let mut chain = OptionChain::new(canonical_sym.clone(), spot);
    let underlying_sym: Symbol = canonical_sym
        .underlying
        .as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| canonical_sym.clone());
    let market = Market::usa();

    for bar in bars {
        if bar.expiration < today {
            continue;
        } // include 0DTE (expiration == today)
        if bar.strike < Decimal::ONE {
            continue;
        }

        let right = match bar.right.to_ascii_lowercase().as_str() {
            "c" | "call" => OptionRight::Call,
            "p" | "put" => OptionRight::Put,
            _ => continue,
        };

        let sym = Symbol::create_option_osi(
            underlying_sym.clone(),
            bar.strike,
            bar.expiration,
            right,
            OptionStyle::American,
            &market,
        );

        let mid = if bar.bid > Decimal::ZERO && bar.ask > Decimal::ZERO {
            (bar.bid + bar.ask) / rust_decimal_macros::dec!(2)
        } else {
            bar.close
        };
        let last = if bar.close > Decimal::ZERO {
            bar.close
        } else {
            mid
        };

        let mut contract = OptionContract::new(sym);
        contract.data = OptionContractData {
            underlying_last_price: spot,
            bid_price: bar.bid,
            ask_price: bar.ask,
            last_price: last,
            volume: bar.volume,
            bid_size: bar.bid_size,
            ask_size: bar.ask_size,
            ..Default::default()
        };
        chain.add_contract(contract);
    }

    reprice_option_chain(&mut chain, valuation_time);
    chain
}

/// Check the local Parquet cache for option EOD bars; download and cache on miss.
///
/// Called from inside `tokio::task::spawn_blocking` — all I/O here is synchronous.
///
/// Cache layout: `{data_root}/option/usa/daily/{ticker_lower}/{YYYYMMDD}_eod.parquet`
/// One file per (ticker, date). A cache hit is a single `path.exists()` check.
fn load_option_eod_bars(
    data_root: &Path,
    ticker: &str,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<OptionEodBar> {
    let cache_path = option_eod_path(data_root, ticker, date);

    // Cache hit — read and return.
    if cache_path.exists() {
        let reader = ParquetReader::new();
        return reader
            .read_option_eod_bars(&[cache_path])
            .unwrap_or_default();
    }

    // Cache miss — fetch from provider.
    let Some(provider) = provider else {
        return vec![];
    };
    let bars = match provider.get_option_eod_bars(ticker, date) {
        Ok(b) => b,
        Err(e) => {
            warn!("option EOD fetch failed for {ticker} {date}: {e}");
            return vec![];
        }
    };

    if bars.is_empty() {
        return vec![];
    }

    // Write to cache so future backtests don't re-fetch.
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let writer = ParquetWriter::new(WriterConfig::default());
    if let Err(e) = writer.write_option_eod_bars(&bars, &cache_path) {
        warn!("failed to cache option EOD bars for {ticker} {date}: {e}");
    }

    bars
}

/// Load custom data points for one subscription/date.
///
/// Parquet-native providers are read directly from provider parquet paths.
/// Text providers use `get_source()`/`reader()` and persist parsed points as
/// framework parquet under `{data_root}/custom/...`.
async fn load_custom_data_points_for_subscription(
    data_root: PathBuf,
    source_type: String,
    ticker: String,
    date: NaiveDate,
    source: Option<Arc<dyn lean_data_providers::ICustomDataSource>>,
    mut config: CustomDataConfig,
    dynamic_query: lean_data::CustomDataQuery,
) -> Result<Vec<CustomDataPoint>> {
    let effective_query = config.query.merge(&dynamic_query);
    config.query = effective_query.clone();
    config.properties.extend(
        effective_query
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    if let Some(source_ref) = source.as_ref() {
        let source_for_task = Arc::clone(source_ref);
        let ticker_for_task = ticker.clone();
        let config_for_task = config.clone();
        let query_for_task = effective_query.clone();
        let parquet_source = tokio::time::timeout(
            CUSTOM_DATA_SOURCE_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                source_for_task.get_parquet_source(
                    &ticker_for_task,
                    date,
                    &config_for_task,
                    &query_for_task,
                )
            }),
        )
        .await
        .with_context(|| {
            format!(
                "custom parquet source timed out after {:?} for {}/{} {}",
                CUSTOM_DATA_SOURCE_TIMEOUT, source_type, ticker, date
            )
        })?
        .map_err(|e| anyhow::anyhow!("custom parquet source task failed: {e}"))?;

        if let Some(parquet_source) = parquet_source {
            return ParquetReader::new()
                .read_custom_parquet_points(&parquet_source, &effective_query, date)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()));
        }
        if source_ref.is_parquet_native() {
            return Ok(Vec::new());
        }
    }

    let points = tokio::task::spawn_blocking(move || {
        load_custom_data_points(
            &data_root,
            &source_type,
            &ticker,
            date,
            source.as_ref(),
            &config,
        )
    })
    .await
    .unwrap_or_default();
    Ok(points)
}

fn load_custom_data_points(
    data_root: &Path,
    source_type: &str,
    ticker: &str,
    date: NaiveDate,
    source: Option<&Arc<dyn lean_data_providers::ICustomDataSource>>,
    config: &CustomDataConfig,
) -> Vec<CustomDataPoint> {
    let cache_path = custom_data_path(data_root, source_type, ticker, date);

    // Cache hit — read and return.
    if cache_path.exists() {
        let reader = ParquetReader::new();
        return reader
            .read_custom_data_points(&cache_path)
            .unwrap_or_default();
    }

    // Cache miss — need a source plugin to fetch.
    let Some(source) = source else {
        return vec![];
    };

    // Ask plugin where to fetch data for this date.
    let data_source = match source.get_source(ticker, date, config) {
        Some(s) => s,
        None => return vec![], // no data for this date (e.g. weekend)
    };

    // Fetch raw content.
    let raw_content = match data_source.transport {
        CustomDataTransport::Http => {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .user_agent("Mozilla/5.0 (compatible; rlean/0.1)")
                .build()
                .unwrap_or_default();
            match client.get(&data_source.uri).send() {
                Ok(resp) => match resp.text() {
                    Ok(text) => text,
                    Err(e) => {
                        warn!(
                            "custom data fetch body error for {}/{} {}: {}",
                            source_type, ticker, date, e
                        );
                        return vec![];
                    }
                },
                Err(e) => {
                    warn!(
                        "custom data HTTP fetch failed for {}/{} {}: {}",
                        source_type, ticker, date, e
                    );
                    return vec![];
                }
            }
        }
        CustomDataTransport::LocalFile => match std::fs::read_to_string(&data_source.uri) {
            Ok(content) => content,
            Err(e) => {
                warn!(
                    "custom data file read failed for {}/{} {}: {}",
                    source_type, ticker, date, e
                );
                return vec![];
            }
        },
    };

    // Parse content using the plugin's reader() method.
    let mut points: Vec<CustomDataPoint> = Vec::new();
    match data_source.format {
        CustomDataFormat::Csv => {
            // Line-by-line: call reader() on each non-empty line.
            for line in raw_content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(point) = source.reader(line, date, config) {
                    points.push(point);
                }
            }
        }
        CustomDataFormat::Json => {
            // Try to parse as JSON array; call reader() on each serialized element.
            match serde_json::from_str::<serde_json::Value>(&raw_content) {
                Ok(serde_json::Value::Array(arr)) => {
                    for elem in arr {
                        let line = elem.to_string();
                        if let Some(point) = source.reader(&line, date, config) {
                            points.push(point);
                        }
                    }
                }
                Ok(obj) => {
                    // Single JSON object — pass it as a single "line".
                    if let Some(point) = source.reader(&obj.to_string(), date, config) {
                        points.push(point);
                    }
                }
                Err(e) => {
                    warn!(
                        "custom data JSON parse error for {}/{} {}: {}",
                        source_type, ticker, date, e
                    );
                }
            }
        }
    }

    if points.is_empty() {
        return vec![];
    }

    // Write to Parquet cache (bloom filters AND page statistics disabled —
    // parquet-rs 53.x reader panics on TType::Set in metadata with these features).
    let writer = ParquetWriter::new(WriterConfig {
        bloom_filter: false,
        write_statistics: false,
        ..WriterConfig::default()
    });
    if let Err(e) = writer.write_custom_data_points(&points, &cache_path) {
        warn!(
            "failed to cache custom data for {}/{} {}: {}",
            source_type, ticker, date, e
        );
    }

    points
}

/// Apply a factor-file adjustment to a raw bar.
///
/// Looks up `(price_factor, split_factor)` for `bar_date` and scales
/// all OHLCV fields by `price_factor * split_factor`.  Volume is scaled
/// inversely (more shares at lower prices after a split).
/// Return the `(price_factor, split_factor)` that applies to `bar_date`.
///
/// Mirrors the LEAN behavior exercised by the tests here: use the most recent
/// Return the mapped ticker at `date` using LEAN's convention.
///
/// Rows are sorted newest-first; the first row whose date <= query_date is the
/// active mapping for that date.
fn ticker_at_date(rows: &[MapFileEntry], date: NaiveDate) -> Option<&str> {
    rows.iter()
        .find(|r| r.date <= date)
        .map(|r| r.ticker.as_str())
}

/// Return the delisting date from a map file (first / newest row), if the
/// security is delisted.
///
/// LEAN convention: if the newest row's date is before 2049, it is a real
/// delisting date.  Far-future sentinels (year >= 2049) indicate active.
fn delisting_date(rows: &[MapFileEntry]) -> Option<NaiveDate> {
    rows.first().map(|r| r.date).filter(|d| d.year() < 2049)
}

/// factor-file row whose date is strictly earlier than `bar_date`.
/// If no such row exists, return `(1.0, 1.0)`.
fn factor_for_entry(rows: &[FactorFileEntry], bar_date: NaiveDate) -> (f64, f64) {
    if rows.is_empty() {
        return (1.0, 1.0);
    }
    // Most-recent row strictly before bar_date.
    if let Some(row) = rows
        .iter()
        .filter(|r| r.date < bar_date)
        .max_by_key(|r| r.date)
    {
        return (row.price_factor, row.split_factor);
    }
    // bar_date predates every row in the factor file.  Extend the oldest
    // cumulative factor backwards so there is no price discontinuity when the
    // backtest crosses into the period covered by the factor file.
    // Using (1.0, 1.0) here would cause a sudden apparent loss the moment the
    // first factor row became active (holdings bought at raw prices would be
    // re-marked at split/dividend-adjusted prices).
    if let Some(row) = rows.iter().min_by_key(|r| r.date) {
        return (row.price_factor, row.split_factor);
    }
    (1.0, 1.0)
}

fn apply_factor_row(mut bar: TradeBar, rows: &[FactorFileEntry], bar_date: NaiveDate) -> TradeBar {
    let (pf, sf) = factor_for_entry(rows, bar_date);
    let combined = pf * sf;
    if (combined - 1.0).abs() < 1e-9 {
        return bar;
    } // fast-path: no adjustment

    let scale = Decimal::from_f64(combined).unwrap_or(Decimal::ONE);
    bar.open *= scale;
    bar.high *= scale;
    bar.low *= scale;
    bar.close *= scale;
    // Volume scales inversely to price for splits (more shares outstanding).
    if sf != 0.0 && (sf - 1.0).abs() > 1e-9 {
        let vol_scale = Decimal::from_f64(1.0 / sf).unwrap_or(Decimal::ONE);
        bar.volume *= vol_scale;
    }
    bar
}

// ─── factor_for_entry unit tests — mirrors LEAN C# FactorFile.GetPriceFactor ─

#[cfg(test)]
mod factor_tests {
    use super::*;
    use chrono::NaiveDate;
    use lean_storage::schema::FactorFileEntry;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn entry(y: i32, m: u32, day: u32, pf: f64) -> FactorFileEntry {
        FactorFileEntry {
            date: d(y, m, day),
            price_factor: pf,
            split_factor: 1.0,
            reference_price: 0.0,
        }
    }

    fn entry_split(y: i32, m: u32, day: u32, pf: f64, sf: f64) -> FactorFileEntry {
        FactorFileEntry {
            date: d(y, m, day),
            price_factor: pf,
            split_factor: sf,
            reference_price: 0.0,
        }
    }

    /// Factor rows for `test_correctly_determines_price_factors`.
    ///
    /// rlean convention: a row at date D covers bars with `bar_date > D`
    /// (strict).  The factor applies to the BAR AFTER the row date, not on
    /// the row date itself.  This is the inverse of C# LEAN's CSV convention
    /// where the row date is the FIRST day the factor applies.
    ///
    /// Row layout (oldest → newest):
    ///   2023-01-14  pf=0.7  sf=0.125  → combined=0.0875  (split+div)
    ///   2023-10-16  pf=0.8  sf=0.25   → combined=0.2     (split)
    ///   2023-12-24  pf=0.8  sf=0.5    → combined=0.4     (split)
    ///   2023-12-31  pf=0.8  sf=1.0    → combined=0.8     (div)
    ///   2024-01-07  pf=0.9  sf=1.0    → combined=0.9     (div)
    ///   2050-12-31  pf=1.0  sf=1.0    → combined=1.0     (end-of-time sentinel)
    fn make_test_factor_rows() -> Vec<FactorFileEntry> {
        vec![
            entry_split(2023, 1, 14, 0.7, 0.125),
            entry_split(2023, 10, 16, 0.8, 0.25),
            entry_split(2023, 12, 24, 0.8, 0.5),
            entry_split(2023, 12, 31, 0.8, 1.0),
            entry_split(2024, 1, 7, 0.9, 1.0),
            entry_split(2050, 12, 31, 1.0, 1.0),
        ]
    }

    /// SPY-like factor file: entries starting 2021-03-25.
    fn spy_rows() -> Vec<FactorFileEntry> {
        vec![
            entry(2021, 3, 25, 0.9339743),
            entry(2021, 6, 17, 0.9339743),
            entry(2021, 9, 16, 0.9370296),
            entry(2021, 12, 16, 0.9400318),
            entry(2022, 3, 17, 0.9433413),
            entry(2026, 4, 9, 1.0),
        ]
    }

    /// Bar before the first factor file entry → returns oldest row's factor (backward extension).
    /// Previously returned (1.0, 1.0), which caused a phantom loss the moment the backtest
    /// crossed into the period covered by the factor file (prices would suddenly drop).
    #[test]
    fn test_before_first_entry_returns_oldest_factor() {
        let rows = spy_rows();
        let (pf, sf) = factor_for_entry(&rows, d(2020, 10, 16));
        assert!(
            (pf - 0.9339743).abs() < 1e-7,
            "bars before the factor file must extend the oldest factor backward (not 1.0)"
        );
        assert_eq!(sf, 1.0);
    }

    /// Bar exactly on the first entry date → no row strictly before it, so returns oldest
    /// row's factor (same backward-extension path as pre-first-row bars).
    #[test]
    fn test_on_first_entry_date_returns_oldest_factor() {
        let rows = spy_rows();
        let (pf, sf) = factor_for_entry(&rows, d(2021, 3, 25));
        assert!(
            (pf - 0.9339743).abs() < 1e-7,
            "bar on first entry date: no prior row exists, returns oldest row factor"
        );
        assert_eq!(sf, 1.0);
    }

    /// Bar one day after the first entry → picks the first entry's factor.
    #[test]
    fn test_day_after_first_entry() {
        let rows = spy_rows();
        let (pf, sf) = factor_for_entry(&rows, d(2021, 3, 26));
        assert!((pf - 0.9339743).abs() < 1e-7);
        assert_eq!(sf, 1.0);
    }

    /// Bar between two entries → picks the preceding (lower-date) entry.
    #[test]
    fn test_between_entries_picks_preceding() {
        let rows = spy_rows();
        // Between 2021-09-16 (0.9370296) and 2021-12-16 (0.9400318)
        let (pf, _) = factor_for_entry(&rows, d(2021, 11, 1));
        assert!(
            (pf - 0.9370296).abs() < 1e-7,
            "should pick the Sep-16 entry, not the Dec-16 one"
        );
    }

    /// Bar exactly on a non-first entry date → picks the preceding entry.
    #[test]
    fn test_on_middle_entry_date_picks_previous() {
        let rows = spy_rows();
        // On 2021-09-16 exactly → the Sep-16 row itself has date = bar_date,
        // so strict < excludes it; we get the Jun-17 entry (0.9339743).
        let (pf, _) = factor_for_entry(&rows, d(2021, 9, 16));
        assert!(
            (pf - 0.9339743).abs() < 1e-7,
            "bar ON an entry date picks the entry before it (strict <)"
        );
    }

    /// Bar after the last entry (2026-04-09) → picks the 2026-04-09 entry (factor=1.0).
    #[test]
    fn test_after_last_entry_picks_last() {
        let rows = spy_rows();
        let (pf, _) = factor_for_entry(&rows, d(2026, 4, 10));
        assert!(
            (pf - 1.0).abs() < 1e-9,
            "bars after the last entry get factor=1.0"
        );
    }

    /// Jan 4, 2022 must use the 2021-12-16 entry (0.9400318) — matches the
    /// observed LEAN C# value from the real SMA-crossover backtest log.
    #[test]
    fn test_jan_2022_matches_lean_observed() {
        let rows = spy_rows();
        let (pf, _) = factor_for_entry(&rows, d(2022, 1, 4));
        assert!(
            (pf - 0.9400318).abs() < 1e-7,
            "2022-01-04 must use the 2021-12-16 factor (0.9400318)"
        );
    }

    /// Empty rows → always 1.0.
    #[test]
    fn test_empty_rows() {
        assert_eq!(factor_for_entry(&[], d(2020, 1, 1)), (1.0, 1.0));
    }

    // ── C# LEAN parity tests ──────────────────────────────────────────────────
    //
    // These mirror the assertions in LEAN's FactorFileTests.CorrectlyDeterminesTimePriceFactors
    // (Lean/Tests/Common/Data/Auxiliary/FactorFileTests.cs) adapted to rlean's Parquet
    // convention.
    //
    // rlean convention vs C# CSV convention:
    //   C# row at date D → factor applies to bars WHERE bar_date >= D
    //   rlean row at date D → factor applies to bars WHERE bar_date > D  (strict)
    //
    // To obtain the same economic result, rlean row dates are one calendar day
    // EARLIER than the corresponding C# CSV row date.  E.g., a C# row at 2024-01-08
    // (dividend day) becomes a rlean row at 2024-01-07 (last pre-ex-div day).
    //
    // See make_test_factor_rows() for the data layout.

    /// Mirrors C# CorrectlyDeterminesTimePriceFactors.
    /// The combined price-scale factor (pf * sf) should match C#'s GetPriceFactor
    /// for the adjusted normalization mode.
    #[test]
    fn test_correctly_determines_price_factors() {
        let rows = make_test_factor_rows();

        // Helper: combined PSF for bar on the given date
        let psf = |y, m, day| {
            let (pf, sf) = factor_for_entry(&rows, d(y, m, day));
            pf * sf
        };

        // rlean convention: a row at date D applies to bars with bar_date > D (strictly).
        // Factor ranges (see make_test_factor_rows doc):
        //   bar > 2050-12-31 : 1.0   (sentinel kicks in)
        //   bar 2024-01-08..2050-12-31 : 0.9  (2024-01-07 row)
        //   bar 2024-01-01..2024-01-07 : 0.8  (2023-12-31 row)
        //   bar 2023-12-25..2023-12-31 : 0.4  (2023-12-24 row, split 2:1 applied)
        //   bar 2023-10-17..2023-12-24 : 0.2  (2023-10-16 row, split 4:1 applied)
        //   bar <= 2023-01-14          : 0.0875 (oldest row, backward extension)

        // After last real action (before sentinel) → still that action's factor
        assert!(
            (psf(2024, 1, 9) - 0.9).abs() < 1e-9,
            "bar after last action row → 0.9"
        );
        assert!(
            (psf(2024, 1, 8) - 0.9).abs() < 1e-9,
            "day after last action row → 0.9"
        );

        // ON the last action row date → falls back to prev row
        assert!(
            (psf(2024, 1, 7) - 0.8).abs() < 1e-9,
            "ON 2024-01-07 row → prev row 0.8"
        );

        // Between 2023-12-31 and 2024-01-07 rows → 2023-12-31 row (div, sf=1.0)
        assert!(
            (psf(2024, 1, 6) - 0.8).abs() < 1e-9,
            "2024-01-06 → 2023-12-31 row 0.8"
        );
        assert!(
            (psf(2024, 1, 1) - 0.8).abs() < 1e-9,
            "2024-01-01 → 2023-12-31 row 0.8"
        );

        // ON 2023-12-31 row → falls back to 2023-12-24 row (split 2:1, combined=0.4)
        assert!(
            (psf(2023, 12, 31) - 0.4).abs() < 1e-9,
            "ON 2023-12-31 row → 0.4"
        );

        // Between 2023-12-24 and 2023-12-31 rows → 2023-12-24 (split 2:1, combined=0.4)
        assert!(
            (psf(2023, 12, 30) - 0.4).abs() < 1e-9,
            "2023-12-30 → 2023-12-24 row 0.4"
        );
        assert!(
            (psf(2023, 12, 25) - 0.4).abs() < 1e-9,
            "2023-12-25 → 2023-12-24 row 0.4"
        );

        // ON 2023-12-24 row → falls to 2023-10-16 (split 4:1, combined=0.2)
        assert!(
            (psf(2023, 12, 24) - 0.2).abs() < 1e-9,
            "ON 2023-12-24 row → 0.2"
        );

        // Between 2023-10-16 and 2023-12-24 rows → 2023-10-16 row (split 4:1, combined=0.2)
        assert!(
            (psf(2023, 12, 1) - 0.2).abs() < 1e-9,
            "2023-12-01 → 2023-10-16 row 0.2"
        );
        assert!(
            (psf(2023, 10, 17) - 0.2).abs() < 1e-9,
            "2023-10-17 → 2023-10-16 row 0.2"
        );

        // ON 2023-10-16 row → falls to 2023-01-14 (oldest row, combined=0.0875)
        assert!(
            (psf(2023, 10, 16) - 0.0875).abs() < 1e-9,
            "ON 2023-10-16 row → 0.0875"
        );

        // Between first row and 2023-10-16 row → 2023-01-14 row (combined=0.0875)
        assert!(
            (psf(2023, 5, 1) - 0.0875).abs() < 1e-9,
            "2023-05-01 → first row 0.0875"
        );
        assert!(
            (psf(2023, 1, 15) - 0.0875).abs() < 1e-9,
            "day after first row → 0.0875"
        );

        // ON first row date and before → backward extension of oldest factor
        assert!(
            (psf(2023, 1, 14) - 0.0875).abs() < 1e-9,
            "ON first row → backward ext 0.0875"
        );
        assert!(
            (psf(2020, 1, 1) - 0.0875).abs() < 1e-9,
            "before first row → backward ext 0.0875"
        );
    }

    /// Mirrors C# HasSplitEventOnNextTradingDay.
    /// In rlean, a split row at date D (split_factor != 1) means the split
    /// took effect for bars dated D+1 and later.  The bar ON date D still
    /// uses the pre-split row (because `max(date < D)` = previous row).
    /// So the bar date when split first appears is D+1.
    #[test]
    fn test_split_detected_at_correct_bar_dates() {
        let rows = make_test_factor_rows();

        // 2023-12-24 row: sf=0.5 (2:1 split).  Appears for the first time on bar 2023-12-25.
        let (_, sf_before) = factor_for_entry(&rows, d(2023, 12, 24)); // ON row date → prev row
        let (_, sf_on_plus_one) = factor_for_entry(&rows, d(2023, 12, 25)); // 1 day after → this row
        assert!(
            (sf_before - 0.25).abs() < 1e-9,
            "day before split takes effect: sf=0.25"
        );
        assert!(
            (sf_on_plus_one - 0.5).abs() < 1e-9,
            "day split takes effect: sf=0.5"
        );

        // 2023-10-16 row: sf=0.25 (4:1 split).  First appears on bar 2023-10-17.
        let (_, sf_before2) = factor_for_entry(&rows, d(2023, 10, 16));
        let (_, sf_after2) = factor_for_entry(&rows, d(2023, 10, 17));
        assert!(
            (sf_before2 - 0.125).abs() < 1e-9,
            "before 4:1 split: sf=0.125"
        );
        assert!((sf_after2 - 0.25).abs() < 1e-9, "after 4:1 split: sf=0.25");
    }

    /// Mirrors C# HasDividendEventOnNextTradingDay.
    /// Dividend rows have sf=1.0; the price_factor drops to reflect the dividend.
    #[test]
    fn test_dividend_detected_at_correct_bar_dates() {
        let rows = make_test_factor_rows();

        // 2024-01-07 row: pf=0.9, sf=1.0 (dividend).
        // Bar on 2024-01-07 → uses prev row (pf=0.8), bar on 2024-01-08 → uses this row (pf=0.9).
        let (pf_on_row, _) = factor_for_entry(&rows, d(2024, 1, 7));
        let (pf_next_day, _) = factor_for_entry(&rows, d(2024, 1, 8));
        assert!(
            (pf_on_row - 0.8).abs() < 1e-9,
            "on div row date: still old pf=0.8"
        );
        assert!(
            (pf_next_day - 0.9).abs() < 1e-9,
            "day after div row: new pf=0.9"
        );
    }

    /// Split factor backward extension:  if the backtest starts before any split occurred,
    /// price continuity requires extending the oldest cumulative factor (which already
    /// encodes all historical splits) back to the dawn of time.
    #[test]
    fn test_split_factor_extends_backward_before_first_row() {
        let rows = make_test_factor_rows();
        // The oldest row (2023-01-14) has sf=0.125 (cumulative of all historical splits).
        // Bars from 1990 through 2023-01-14 must also see sf=0.125, not sf=1.0.
        let (pf, sf) = factor_for_entry(&rows, d(1990, 1, 1));
        assert!((pf - 0.7).abs() < 1e-9);
        assert!((sf - 0.125).abs() < 1e-9);
    }

    /// apply_factor_row: a 2:1 split (sf=0.5) on bar after row date halves the price
    /// and doubles the volume.
    #[test]
    fn test_apply_factor_row_scales_volume_for_split() {
        use lean_data::trade_bar::TradeBarData;
        use rust_decimal_macros::dec;

        // A 2:1 split (sf=0.5) should DOUBLE the volume on pre-split bars.
        let rows = vec![
            entry_split(2023, 12, 24, 1.0, 0.5),
            entry_split(2050, 12, 31, 1.0, 1.0),
        ];

        let sym = lean_core::Symbol::create_equity("SPY", &lean_core::Market::usa());
        let bar = TradeBar::new(
            sym,
            lean_core::NanosecondTimestamp(0),
            lean_core::TimeSpan::from_days(1),
            TradeBarData::new(dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000)),
        );

        // bar on 2023-12-25 (one day after the row): split factor 0.5 applies.
        let adjusted = apply_factor_row(bar.clone(), &rows, d(2023, 12, 25));
        assert_eq!(adjusted.close, dec!(52.5)); // 105 * 0.5
        assert_eq!(adjusted.volume, dec!(2000)); // 1000 / 0.5
    }
}

// ─── benchmark unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lean_algorithm::qc_algorithm::QcAlgorithm;
    use lean_core::SymbolOptionsExt;
    use lean_data::quote_bar::Bar;
    use rust_decimal_macros::dec;

    /// Helper: create a fresh QcAlgorithm with default (no) benchmark.
    fn make_alg() -> QcAlgorithm {
        QcAlgorithm::new("test", dec!(100_000))
    }

    #[test]
    fn default_benchmark_is_spy_when_not_set() {
        let alg = make_alg();
        // benchmark_symbol is None → runner defaults to SPY
        assert!(alg.benchmark_symbol.is_none());
        let effective = alg.benchmark_symbol.unwrap_or_else(|| "SPY".to_string());
        assert_eq!(effective, "SPY");
    }

    #[test]
    fn set_benchmark_overrides_default() {
        let mut alg = make_alg();
        alg.set_benchmark("QQQ");
        let effective = alg.benchmark_symbol.unwrap_or_else(|| "SPY".to_string());
        assert_eq!(effective, "QQQ");
    }

    #[test]
    fn set_benchmark_uppercases_ticker() {
        let mut alg = make_alg();
        alg.set_benchmark("qqq");
        assert_eq!(alg.benchmark_symbol.as_deref(), Some("QQQ"));
    }

    #[test]
    fn benchmark_returns_computed_from_price_series() {
        // A price series of [100, 110, 99] corresponds to daily returns of
        // [(110-100)/100 = 10%, (99-110)/110 ≈ -10%].
        let prices: Vec<Decimal> = vec![dec!(100), dec!(110), dec!(99)];
        let returns: Vec<Decimal> = prices.windows(2).map(|w| (w[1] - w[0]) / w[0]).collect();

        // 10% up
        let expected_up = dec!(10) / dec!(100);
        assert!((returns[0] - expected_up).abs() < dec!(0.0001));

        // ≈ -10% down
        let expected_down = (dec!(99) - dec!(110)) / dec!(110);
        assert!((returns[1] - expected_down).abs() < dec!(0.0001));
    }

    #[test]
    fn benchmark_symbol_appears_in_backtest_result_field() {
        // Verify the BacktestResult struct carries the benchmark ticker.
        // We build a minimal BacktestResult directly.
        use crate::charting::ChartCollection;
        use lean_statistics::PortfolioStatistics;

        let stats = PortfolioStatistics::compute(
            &[dec!(100_000), dec!(101_000)],
            &[dec!(400), dec!(402)],
            &[],
            1,
            dec!(100_000),
            dec!(0),
        );
        let result = BacktestResult {
            trading_days: 1,
            final_value: 101_000.0,
            total_return: 0.01,
            starting_cash: 100_000.0,
            start_date: chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            end_date: chrono::NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
            equity_curve: vec![100_000.0, 101_000.0],
            daily_dates: vec!["2024-01-02".to_string(), "2024-01-03".to_string()],
            statistics: stats,
            charts: ChartCollection::default(),
            order_events: vec![],
            succeeded_data_requests: vec![],
            failed_data_requests: vec![],
            backtest_id: 1_700_000_000,
            benchmark_symbol: "QQQ".to_string(),
        };
        assert_eq!(result.benchmark_symbol, "QQQ");
    }

    #[test]
    fn benchmark_symbol_for_spy_equity() {
        // Verify that SPY symbol creation is stable and has the correct SID.
        let market = lean_core::Market::usa();
        let spy_a = Symbol::create_equity("SPY", &market);
        let spy_b = Symbol::create_equity("SPY", &market);
        // Two independently created SPY symbols must have the same SID so the
        // benchmark price map lookup works correctly.
        assert_eq!(spy_a.id.sid, spy_b.id.sid);
        assert_eq!(spy_a.permtick, "SPY");
    }

    #[test]
    fn benchmark_in_subs_detected_correctly() {
        use lean_core::{Market, Resolution, Symbol};
        use lean_data::SubscriptionDataConfig;
        use std::sync::Arc;

        let market = Market::usa();
        let spy = Symbol::create_equity("SPY", &market);

        let cfg_spy = Arc::new(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Daily,
        ));
        let subs = [cfg_spy];

        // SPY is in subs → benchmark_in_subs = true
        let benchmark_ticker = "SPY";
        let in_subs = subs
            .iter()
            .any(|s| s.symbol.permtick.eq_ignore_ascii_case(benchmark_ticker));
        assert!(in_subs);

        // QQQ is NOT in subs → benchmark_in_subs = false
        let benchmark_ticker2 = "QQQ";
        let in_subs2 = subs
            .iter()
            .any(|s| s.symbol.permtick.eq_ignore_ascii_case(benchmark_ticker2));
        assert!(!in_subs2);
    }

    #[test]
    fn build_option_chain_from_eod_bars_populates_model_data() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let valuation_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 20, 0, 0)
                .single()
                .unwrap(),
        );

        let bars = vec![OptionEodBar {
            date: valuation_time.date_utc(),
            symbol_value: "SPY240119C00100000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(100),
            right: "C".to_string(),
            open: dec!(2.50),
            high: dec!(2.60),
            low: dec!(2.40),
            close: dec!(2.50),
            volume: 42,
            bid: dec!(2.40),
            ask: dec!(2.60),
            bid_size: 10,
            ask_size: 12,
        }];

        let chain = build_option_chain_from_eod_bars(&canonical, dec!(100), valuation_time, &bars);
        let contract = chain.contracts.values().next().unwrap();

        assert!(contract.data.implied_volatility > Decimal::ZERO);
        assert!(contract.data.theoretical_price > Decimal::ZERO);
        assert!(contract.data.greeks.delta > Decimal::ZERO);
    }

    #[test]
    fn option_chain_runtime_reprices_from_latest_quote_and_current_underlying() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();

        let rows = vec![OptionUniverseRow {
            date: chrono::NaiveDate::from_ymd_opt(2024, 1, 18).unwrap(),
            symbol_value: "SPY240119C00100000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(100),
            right: "C".to_string(),
        }];
        let chain = build_option_chain_from_universe_rows(&canonical, dec!(100), &rows);
        let contract_symbol = chain.contracts.keys().next().unwrap().clone();

        let first_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 15, 0, 0)
                .single()
                .unwrap(),
        );
        let second_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 15, 1, 0)
                .single()
                .unwrap(),
        );

        let quote_bar = QuoteBar::new(
            contract_symbol.clone(),
            first_time,
            TimeSpan::ONE_MINUTE,
            Some(Bar::from_price(dec!(2.40))),
            Some(Bar::from_price(dec!(2.60))),
            dec!(10),
            dec!(12),
        );

        let mut runtime = OptionChainRuntime {
            permtick: canonical.permtick.clone(),
            chain,
            trade_updates: HashMap::new(),
            quote_updates: HashMap::from([(first_time.0, vec![quote_bar])]),
            tick_updates: HashMap::new(),
        };

        runtime.apply_timestamp(first_time, dec!(100));
        let initial_delta = runtime
            .chain
            .contracts
            .get(&contract_symbol)
            .unwrap()
            .data
            .greeks
            .delta;

        runtime.apply_timestamp(second_time, dec!(105));
        let repriced = runtime.chain.contracts.get(&contract_symbol).unwrap();

        assert!(repriced.data.implied_volatility > Decimal::ZERO);
        assert!(repriced.data.theoretical_price > Decimal::ZERO);
        assert!(repriced.data.greeks.delta > initial_delta);
    }
}
