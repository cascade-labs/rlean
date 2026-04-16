/// Standalone Python strategy runner.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json;
use chrono::Datelike;
use chrono::NaiveDate;
use pyo3::prelude::*;
use pyo3::types::PyType;
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive as _;
use tracing::{info, warn};

use lean_algorithm::algorithm::IAlgorithm;
use lean_core::{DateTime, Market, OptionRight, OptionStyle, Resolution, Symbol, SymbolOptionsExt, TimeSpan};
use lean_data::{CustomDataConfig, CustomDataFormat, CustomDataPoint, CustomDataSource, CustomDataSubscription, CustomDataTransport, IHistoricalDataProvider, QuoteBar, Slice, SubscriptionDataConfig, TradeBar};
use lean_options::{OptionChain, OptionContract, OptionContractData};
use lean_options::payoff::{intrinsic_value, is_auto_exercised, get_exercise_quantity};
use lean_orders::{
    fee_model::{FeeModel, InteractiveBrokersFeeModel, OrderFeeParameters},
    fill_model::ImmediateFillModel,
    order_event::OrderEvent,
    order_processor::OrderProcessor,
    slippage::NullSlippageModel,
};
use lean_statistics::{PortfolioStatistics, Trade};
use lean_storage::{DataCache, FactorFileEntry, OptionEodBar, ParquetReader, ParquetWriter, PathResolver, QueryParams, WriterConfig, custom_data_path, custom_data_history_path, option_eod_path};

use crate::charting::ChartCollection;
use crate::py_adapter::{PyAlgorithmAdapter, set_algorithm_time};
use crate::py_data::SliceProxy;
use crate::py_qc_algorithm::PyQcAlgorithm;

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
        }
    }
}

pub struct BacktestResult {
    pub trading_days:  i64,
    pub final_value:   f64,
    pub total_return:  f64,
    pub starting_cash: f64,
    pub start_date:    chrono::NaiveDate,
    pub end_date:      chrono::NaiveDate,
    /// Daily portfolio values (one per trading day, in order).
    pub equity_curve:  Vec<f64>,
    /// ISO date strings matching equity_curve.
    pub daily_dates:   Vec<String>,
    /// Full statistics computed at the end of the backtest.
    pub statistics:    PortfolioStatistics,
    /// Custom strategy charts plotted via self.plot().
    pub charts:        ChartCollection,
    /// All order fill events from the backtest run.
    pub order_events:  Vec<OrderEvent>,
    /// Symbols/dates for which data was found in the Parquet store.
    pub succeeded_data_requests: Vec<String>,
    /// Symbols/dates for which no data was found.
    pub failed_data_requests: Vec<String>,
    /// Unix epoch seconds at backtest start (used as backtest ID).
    pub backtest_id: i64,
    /// The ticker used as the benchmark (e.g. "SPY").
    pub benchmark_symbol: String,
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
        row("Start Date",             &self.start_date.to_string());
        row("End Date",               &self.end_date.to_string());
        row("Trading Days",           &self.trading_days.to_string());
        row("Starting Cash",          &format!("${:.2}", self.starting_cash));
        row("Final Value",            &format!("${:.2}", self.final_value));
        row("Total Return",           &format!("{:.2}%", self.total_return * 100.0));
        row("CAGR",                   &format!("{:.2}%", s.compounding_annual_return.to_f64().unwrap_or(0.0) * 100.0));
        row("Sharpe Ratio",           &format!("{:.3}", s.sharpe_ratio.to_f64().unwrap_or(0.0)));
        row("Sortino Ratio",          &format!("{:.3}", s.sortino_ratio.to_f64().unwrap_or(0.0)));
        row("Probabilistic SR",       &format!("{:.1}%", s.probabilistic_sharpe_ratio.to_f64().unwrap_or(0.0) * 100.0));
        row("Calmar Ratio",           &format!("{:.3}", s.calmar_ratio.to_f64().unwrap_or(0.0)));
        row("Omega Ratio",            &format!("{:.3}", s.omega_ratio.to_f64().unwrap_or(0.0)));
        row("Max Drawdown",           &format!("{:.2}%", s.drawdown.to_f64().unwrap_or(0.0) * 100.0));
        row("Recovery Factor",        &format!("{:.2}", s.recovery_factor.to_f64().unwrap_or(0.0)));
        row("Annual Std Dev",         &format!("{:.2}%", s.annual_standard_deviation.to_f64().unwrap_or(0.0) * 100.0));
        row("Alpha",                  &format!("{:.2}%", s.alpha.to_f64().unwrap_or(0.0) * 100.0));
        row("Beta",                   &format!("{:.3}", s.beta.to_f64().unwrap_or(0.0)));
        row("Treynor Ratio",          &format!("{:.3}", s.treynor_ratio.to_f64().unwrap_or(0.0)));
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
    path_list.call_method1("insert", (0, parent.to_string_lossy().as_ref()))
        .context("failed to insert to sys.path")?;

    // Read and compile the strategy source.
    let code_str = std::fs::read_to_string(strategy_path)
        .with_context(|| format!("cannot read {}", strategy_path.display()))?;
    let filename_str = strategy_path.to_string_lossy().to_string();

    // pyo3 0.23 requires &CStr
    use std::ffi::CString;
    let code_c     = CString::new(code_str.as_str()).context("strategy code contains null byte")?;
    let filename_c = CString::new(filename_str.as_str()).context("filename contains null byte")?;
    let modname_c  = CString::new("strategy").unwrap();

    let module = PyModule::from_code(py, &code_c, &filename_c, &modname_c)
        .with_context(|| format!("failed to compile {}", strategy_path.display()))?;

    // Get the QCAlgorithm base class from the AlgorithmImports module.
    // Try AlgorithmImports first (new name), fall back to lean_rust (old name).
    let lean_mod = py.import("AlgorithmImports")
        .or_else(|_| py.import("lean_rust"))
        .context("AlgorithmImports not importable — was append_to_inittab!(lean_python::AlgorithmImports) called before prepare_freethreaded_python()?")?;
    let base_class = lean_mod.getattr("QCAlgorithm")
        .or_else(|_| lean_mod.getattr("QcAlgorithm"))
        .context("QCAlgorithm not found in AlgorithmImports")?;

    // Walk the module namespace to find the first QcAlgorithm subclass.
    let builtins = py.import("builtins")?;
    let issubclass_fn = builtins.getattr("issubclass")?;

    let mut strategy_class: Option<Bound<'_, PyAny>> = None;
    for (_, value) in module.dict() {
        if !value.is_instance_of::<PyType>() { continue; }
        if value.eq(&base_class).unwrap_or(false) { continue; }

        let is_sub = issubclass_fn
            .call1((&value, &base_class))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false);

        if is_sub {
            let name = value.getattr("__name__")
                .map(|n| n.to_string())
                .unwrap_or_default();
            info!("Found strategy class: {}", name);
            strategy_class = Some(value);
            break;
        }
    }

    let cls = strategy_class
        .ok_or_else(|| anyhow::anyhow!(
            "No QcAlgorithm subclass found in {}", strategy_path.display()
        ))?;

    let instance    = cls.call0().context("failed to instantiate strategy class")?;
    let instance_py = instance.unbind();

    PyAlgorithmAdapter::from_instance(py, instance_py)
        .context("strategy class must inherit from AlgorithmImports.QCAlgorithm")
}

/// Run the full backtest loop for a Python strategy.
///
/// Must be called from within an existing tokio runtime (e.g. via `.await`).
/// Do NOT decorate call-sites with `#[tokio::main]` — the caller's runtime
/// is reused so that tokio primitives (Mutex, Semaphore, reqwest) in the
/// historical provider work correctly across the same runtime context.
pub async fn run_strategy(strategy_path: &Path, config: RunConfig) -> Result<BacktestResult> {
    let mut adapter = Python::with_gil(|py| load_strategy(py, strategy_path))?;

    // ── initialize ──────────────────────────────────────────────────────────
    adapter.initialize().context("strategy initialize() failed")?;

    let start_date = config.start_date_override.unwrap_or_else(|| adapter.start_date().date_utc());
    let end_date   = config.end_date_override.unwrap_or_else(|| adapter.end_date().date_utc());

    let starting_cash = {
        use rust_decimal::prelude::ToPrimitive;
        adapter.inner.lock().unwrap().portfolio_value().to_f64().unwrap_or(100_000.0)
    };

    // ── gather subscriptions ────────────────────────────────────────────────
    let subscriptions: Vec<Arc<SubscriptionDataConfig>> = {
        adapter.inner.lock().unwrap().subscription_manager.get_all()
    };

    if subscriptions.is_empty() {
        warn!("No subscriptions — strategy did not call add_equity/add_forex.");
    }

    // ── determine effective benchmark ticker ────────────────────────────────
    // Use the symbol set by set_benchmark(), or fall back to SPY.
    let effective_benchmark_ticker: String = {
        adapter.inner.lock().unwrap()
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
    let benchmark_in_subs: bool = subscriptions.iter()
        .any(|s| s.symbol.permtick.eq_ignore_ascii_case(&effective_benchmark_ticker));

    info!(
        "Benchmark: {} ({})",
        effective_benchmark_ticker,
        if benchmark_in_subs { "already subscribed" } else { "internal subscription" }
    );

    // ── build infrastructure ────────────────────────────────────────────────
    let reader      = Arc::new(ParquetReader::new());
    let resolver    = PathResolver::new(config.data_root.clone());
    let cache       = DataCache::new(50_000);
    let transactions = adapter.inner.lock().unwrap().transactions.clone();
    let portfolio    = adapter.inner.lock().unwrap().portfolio.clone();

    let order_processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        transactions,
    );

    // ── pre-fetch missing data ───────────────────────────────────────────────
    if let Some(ref provider) = config.historical_provider {
        pre_fetch_all(
            provider.as_ref(),
            config.history_provider.clone(),
            &subscriptions,
            start_date,
            end_date,
            &resolver,
        ).await?;
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
        let sec    = format!("{}", sub.symbol.security_type()).to_lowercase();
        let factor_path = config.data_root
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
    // Determine the warm-up window from the strategy's configuration.
    // The strategy may call set_warm_up(days) or set_warm_up_bars(n) in
    // initialize().  Both are stored as `warmup_bar_count` / `warmup_duration`
    // on QcAlgorithm.  We also honour the legacy `warmup_period` field.
    let warmup_start: Option<NaiveDate> = {
        let alg = adapter.inner.lock().unwrap();
        if let Some(bar_count) = alg.warmup_bar_count {
            // Simple heuristic: 1 bar ≈ 1 calendar day for daily data.
            let days = bar_count as i64;
            Some(start_date - chrono::Duration::days(days))
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

    if let Some(wu_start) = warmup_start {
        info!("Warm-up: {} → {} (exclusive)", wu_start, start_date);

        // Pre-fetch warm-up data if a historical provider is configured.
        if let Some(ref provider) = config.historical_provider {
            pre_fetch_all(
                provider.as_ref(),
                config.history_provider.clone(),
                &subscriptions,
                wu_start,
                start_date - chrono::Duration::days(1),
                &resolver,
            ).await?;
        }

        let mut wu_date = wu_start;
        while wu_date < start_date {
            let utc_time = date_to_datetime(wu_date, 16, 0, 0);
            set_algorithm_time(&adapter, utc_time);

            let mut slice = Slice::new(utc_time);
            for sub in &subscriptions {
                let sid     = sub.symbol.id.sid;
                let day_key = day_key(wu_date);
                let path    = resolver.trade_bar(&sub.symbol, sub.resolution, wu_date).to_path();

                if path.exists() {
                    let bars = if let Some(cached) = cache.get_bars(sid, day_key) {
                        cached.as_ref().clone()
                    } else {
                        let day_start = date_to_datetime(wu_date, 0, 0, 0);
                        let day_end   = date_to_datetime(wu_date, 23, 59, 59);
                        let params    = QueryParams::new().with_time_range(day_start, day_end);
                        let loaded    = reader
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
                        adapter.inner.lock().unwrap()
                            .securities.update_price(&bar.symbol, bar.close);
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
    let is_intraday = subscriptions.iter().any(|s| s.resolution.is_high_resolution());

    // ── pre-load all subscription bars (daily mode only) ─────────────────────
    // For daily (and other single-file) resolutions the same parquet file would
    // be opened and scanned once per trading day in the loop.  Pre-loading the
    // full date range up front reduces 629 file reads to 1 per subscription.
    let bar_map: HashMap<u64, HashMap<chrono::NaiveDate, lean_data::TradeBar>> = if !is_intraday {
        let full_params = QueryParams::new()
            .with_time_range(date_to_datetime(start_date, 0, 0, 0),
                             date_to_datetime(end_date, 23, 59, 59));
        let mut map = HashMap::new();
        for sub in &subscriptions {
            let sid  = sub.symbol.id.sid;
            let path = resolver.trade_bar(&sub.symbol, sub.resolution, start_date).to_path();
            if path.exists() {
                let bars = reader
                    .read_trade_bars(&[path], sub.symbol.clone(), &full_params)
                    .await
                    .unwrap_or_default();
                let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> = bars
                    .into_iter()
                    .map(|b| (b.time.date_utc(), b))
                    .collect();
                info!("Pre-loaded {} bars for {}", date_map.len(), sub.symbol.value);
                map.insert(sid, date_map);
            }
        }
        map
    } else {
        HashMap::new()
    };

    // ── pre-allocate proxy objects for the hot path ──────────────────────────
    // One PyTradeBar per subscription is allocated here and reused every day.
    // `on_data_proxy` updates fields in-place instead of constructing new objects.
    let slice_proxy = Python::with_gil(|py| SliceProxy::new(py, &subscriptions))
        .context("Failed to create SliceProxy")?;

    // ── pre-load benchmark data ──────────────────────────────────────────────
    let benchmark_sid: u64 = benchmark_symbol_obj.id.sid;
    let mut benchmark_curve: Vec<Decimal> = Vec::new();
    let benchmark_price_map: HashMap<NaiveDate, Decimal> = {
        let mut map: HashMap<NaiveDate, Decimal> = HashMap::new();
        if !benchmark_in_subs {
            let bm_sym = benchmark_symbol_obj.clone();
            let day_start = date_to_datetime(start_date, 0, 0, 0);
            let day_end   = date_to_datetime(end_date, 23, 59, 59);
            let params    = QueryParams::new().with_time_range(day_start, day_end);
            let bm_path = resolver.trade_bar(&bm_sym, Resolution::Daily, start_date).to_path();
            if bm_path.exists() {
                match reader.read_trade_bars(&[bm_path], bm_sym.clone(), &params).await {
                    Ok(bars) => {
                        for b in bars {
                            let d = b.time.date_utc();
                            map.insert(d, b.close);
                        }
                        info!(
                            "Loaded {} benchmark price points for {}",
                            map.len(), effective_benchmark_ticker
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
                    resolver.trade_bar(&bm_sym, Resolution::Daily, start_date).to_path().display()
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
        let subs: Vec<CustomDataSubscription> = adapter.inner.lock().unwrap()
            .custom_data_subscriptions.clone();
        let mut out: HashMap<String, HashMap<NaiveDate, Vec<CustomDataPoint>>> = HashMap::new();

        // Build a single async HTTP client for all downloads.
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .user_agent("Mozilla/5.0 (compatible; rlean/0.1)")
            .build()
            .unwrap_or_default();

        for sub in &subs {
            let Some(source) = config.custom_data_sources.iter().find(|s| s.name() == sub.source_type) else {
                continue;
            };
            if !source.is_full_history_source() {
                continue;
            }
            let history_path = custom_data_history_path(&config.data_root, &sub.source_type, &sub.ticker);

            // Try reading from existing on-disk cache first (synchronous, fast).
            let all_points: Vec<CustomDataPoint> = if history_path.exists() {
                let hp = history_path.clone();
                tokio::task::spawn_blocking(move || {
                    ParquetReader::new().read_custom_data_points(&hp).unwrap_or_default()
                }).await.unwrap_or_default()
            } else {
                // Download full series using async HTTP.
                let data_source = match source.get_source(&sub.ticker, NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(), &sub.config) {
                    Some(s) => s,
                    None => { warn!("custom data: get_source returned None for {}/{}", sub.source_type, sub.ticker); continue; }
                };
                let raw = match data_source.transport {
                    lean_data::custom::CustomDataTransport::Http => {
                        // Use curl subprocess: handles HTTP/2, TLS quirks, and redirects
                        // more reliably than reqwest in this environment (some servers
                        // like FRED require HTTP/2 which curl negotiates natively).
                        let output = tokio::process::Command::new("curl")
                            .args([
                                "-s",
                                "--max-time", "120",
                                "-L",  // follow redirects
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
                                warn!("custom data full-history curl failed for {}/{}: {}", sub.source_type, sub.ticker, stderr);
                                continue;
                            }
                            Err(e) => {
                                warn!("custom data full-history download failed for {}/{}: {}", sub.source_type, sub.ticker, e);
                                continue;
                            }
                        }
                    }
                    lean_data::custom::CustomDataTransport::LocalFile => {
                        match std::fs::read_to_string(&data_source.uri) {
                            Ok(t) => t,
                            Err(e) => { warn!("custom data local file read failed for {}/{}: {}", sub.source_type, sub.ticker, e); continue; }
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
                }).await.unwrap_or_default();

                if pts.is_empty() {
                    warn!("custom data: no points parsed for {}/{}", sub.source_type, sub.ticker);
                    continue;
                }
                // Cache to Parquet (off the async thread).
                let hp = history_path.clone();
                let pts_clone = pts.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(parent) = hp.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if let Err(e) = ParquetWriter::new(WriterConfig::default()).write_custom_data_points(&pts_clone, &hp) {
                        warn!("custom data: failed to cache history: {}", e);
                    }
                }).await.ok();
                pts
            };

            info!("custom data: loaded {} history points for {}/{}", all_points.len(), sub.source_type, sub.ticker);
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

    info!("Backtest: {} → {} ({})", start_date, end_date, if is_intraday { "minute" } else { "daily" });

    while current_date <= end_date {
        if is_intraday {
            // ── MINUTE LOOP ──────────────────────────────────────────────────
            // Load minute trade and quote bars from Parquet for each subscription.
            let mut day_trade_bars: HashMap<u64, Vec<lean_data::TradeBar>> = HashMap::new();
            let mut day_quote_bars: HashMap<u64, Vec<QuoteBar>> = HashMap::new();

            let day_params = QueryParams::new()
                .with_time_range(date_to_datetime(current_date, 0, 0, 0),
                                 date_to_datetime(current_date, 23, 59, 59));

            for sub in &subscriptions {
                // In intraday mode skip low-resolution subscriptions (e.g. the daily
                // underlying equity subscription that add_option() creates internally).
                // They share the same SID as the high-resolution subscription and
                // would overwrite the correct minute bars with daily bars.
                if !sub.resolution.is_high_resolution() {
                    continue;
                }

                let sid = sub.symbol.id.sid;
                let trade_path = resolver.trade_bar(&sub.symbol, sub.resolution, current_date).to_path();
                if trade_path.exists() {
                    match reader.read_trade_bars(&[trade_path], sub.symbol.clone(), &day_params).await {
                        Ok(mut bars) if !bars.is_empty() => {
                            // Filter pre-market / after-hours bars for subscriptions where
                            // extended_market_hours=false (the default for US equity).
                            if let Some(hours) = market_hours_filter.get(&sid) {
                                bars.retain(|b| hours.is_open_at(b.time));
                            }
                            // Drop zero-price bars — these are placeholder rows written for
                            // non-trading days (weekends/holidays) by the data provider.
                            bars.retain(|b| b.close > rust_decimal::Decimal::ZERO);
                            if !bars.is_empty() {
                                succeeded_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                                day_trade_bars.insert(sid, bars);
                            } else {
                                failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                            }
                        }
                        Ok(_) => { failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date)); }
                        Err(e) => { warn!("Failed to read minute bars for {} on {}: {}", sub.symbol.value, current_date, e); }
                    }
                } else {
                    failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                }

                // Quote bar loading via ParquetReader not yet implemented; day_quote_bars remains empty.
            }

            // Collect union of all minute timestamps across subscriptions.
            let mut all_timestamps: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
            for bars in day_trade_bars.values() {
                for b in bars { all_timestamps.insert(b.time.0); }
            }

            let has_data = !all_timestamps.is_empty();

            if has_data {
                trading_days += 1;

                // Build per-timestamp lookup: sid → bar at that timestamp
                let mut trade_by_ts: HashMap<u64, HashMap<i64, lean_data::TradeBar>> = HashMap::new();
                for (&sid, bars) in &day_trade_bars {
                    let mut ts_map: HashMap<i64, lean_data::TradeBar> = HashMap::new();
                    for b in bars { ts_map.insert(b.time.0, b.clone()); }
                    trade_by_ts.insert(sid, ts_map);
                }

                let mut quote_by_ts: HashMap<u64, HashMap<i64, QuoteBar>> = HashMap::new();
                for (&sid, qbars) in &day_quote_bars {
                    let mut ts_map: HashMap<i64, QuoteBar> = HashMap::new();
                    for q in qbars { ts_map.insert(q.time.0, q.clone()); }
                    quote_by_ts.insert(sid, ts_map);
                }

                // Fetch and deliver option chains once at the start of each trading day.
                {
                    let option_subs: Vec<Symbol> = {
                        adapter.inner.lock().unwrap().option_subscriptions.clone()
                    };

                    let mut chains_for_day: Vec<(String, OptionChain)> = Vec::new();
                    for canonical in &option_subs {
                        let underlying_ticker = canonical.permtick.trim_start_matches('?');
                        let spot = {
                            adapter.inner.lock().unwrap()
                                .securities.all()
                                .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                                .map(|s| s.current_price())
                                .unwrap_or(Decimal::ZERO)
                        };

                        let chain = {
                            let ticker = underlying_ticker.to_uppercase();
                            let bars = tokio::task::spawn_blocking({
                                let provider = config.history_provider.clone();
                                let data_root = config.data_root.clone();
                                move || load_option_eod_bars(&data_root, &ticker, current_date, provider.as_ref())
                            }).await.unwrap_or_default();
                            if !bars.is_empty() {
                                build_option_chain_from_eod_bars(canonical, spot, current_date, &bars)
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
                    alg.option_chains.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                };

                // Fetch custom data once per day for minute-mode too.
                let custom_subs: Vec<CustomDataSubscription> = {
                    adapter.inner.lock().unwrap()
                        .custom_data_subscriptions.clone()
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
                        let source = config.custom_data_sources.iter()
                            .find(|s| s.name() == sub.source_type)
                            .cloned();
                        let data_root = config.data_root.clone();
                        let source_type = sub.source_type.clone();
                        let ticker = sub.ticker.clone();
                        let cfg = sub.config.clone();
                        let points = tokio::task::spawn_blocking(move || {
                            load_custom_data_points(&data_root, &source_type, &ticker, current_date, source.as_ref(), &cfg)
                        }).await.unwrap_or_default();
                        if !points.is_empty() {
                            custom_data_for_day.insert(sub.ticker.clone(), points);
                        }
                    }
                }

                // Iterate minute by minute.
                for &ts_ns in &all_timestamps {
                    let utc_time = lean_core::NanosecondTimestamp(ts_ns);
                    set_algorithm_time(&adapter, utc_time);

                    // Build a Rust Slice for this minute (used for order processing).
                    let mut minute_slice = Slice::new(utc_time);
                    for sub in &subscriptions {
                        let sid = sub.symbol.id.sid;
                        if let Some(raw_bar) = trade_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            // Apply factor adjustment (splits/dividends) for non-option-underlying equities.
                            let bar = if !option_underlying_sids.contains(&sid) {
                                if let Some(rows) = factor_map.get(&sid) {
                                    apply_factor_row(raw_bar.clone(), rows, current_date)
                                } else {
                                    raw_bar.clone()
                                }
                            } else {
                                raw_bar.clone()
                            };
                            adapter.inner.lock().unwrap()
                                .securities.update_price(&bar.symbol, bar.close);
                            portfolio.update_prices(&bar.symbol, bar.close);
                            minute_slice.add_bar(bar);
                        }
                    }

                    // Gather quote bars for this minute.
                    let minute_quote_bars: HashMap<u64, QuoteBar> = subscriptions.iter()
                        .filter_map(|sub| {
                            let sid = sub.symbol.id.sid;
                            quote_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)).map(|q| (sid, q.clone()))
                        })
                        .collect();

                    // ── ImmediateFillModel semantics (matches C# LEAN default) ──────────────
                    // Deliver the bar to Python first so the algorithm sees the bar's close
                    // price and can submit orders.  Then process those orders immediately at
                    // the current bar's close — identical to C# LEAN's ImmediateFillModel
                    // which fills at `security.Price` (= bar.Close) when SetHoldings is called.
                    Python::with_gil(|py| {
                        slice_proxy.update_option_chains(py, &chains_snapshot);
                        slice_proxy.update_quote_bars(py, &minute_quote_bars);
                        slice_proxy.update_custom_data(py, &custom_data_for_day);
                        adapter.on_data_proxy(py, &slice_proxy, &minute_slice);
                    });

                    // Process orders at this minute using current (close) prices.
                    let bars_for_orders: HashMap<u64, lean_data::TradeBar> = minute_slice.bars
                        .iter()
                        .map(|(&k, v)| (k, v.clone()))
                        .collect();
                    let fill_events = order_processor.process_orders(&bars_for_orders, utc_time);
                    all_order_events.extend(fill_events.iter().cloned());
                    let ib_fee_model = InteractiveBrokersFeeModel::default();
                    for event in &fill_events {
                        if event.is_fill() {
                            if let Some(order) = order_processor.transaction_manager.get_order(event.order_id) {
                                let fee = ib_fee_model.get_order_fee(
                                    &OrderFeeParameters::equity(&order, event.fill_price),
                                ).amount;
                                portfolio.apply_fill(
                                    &order,
                                    event.fill_price,
                                    event.fill_quantity,
                                    fee,
                                );
                            }
                            let sid = event.symbol.id.sid;
                            let fill_qty = event.fill_quantity;
                            if let Some((entry_time, entry_price, open_qty)) = open_positions.remove(&sid) {
                                let close_qty = open_qty.abs().min(fill_qty.abs());
                                completed_trades.push(Trade::new(
                                    event.symbol.clone(),
                                    entry_time,
                                    event.utc_time,
                                    entry_price,
                                    event.fill_price,
                                    close_qty,
                                    rust_decimal_macros::dec!(0),
                                ));
                            } else {
                                open_positions.insert(sid, (event.utc_time, event.fill_price, fill_qty));
                            }
                        }
                        adapter.on_order_event(event);
                    }
                }

                // End-of-day calls.
                adapter.on_end_of_day(None);
                process_option_expirations(&mut adapter, current_date);

                // Record benchmark close for this day.
                let bm_close: Option<Decimal> = if benchmark_in_subs {
                    // Look up benchmark from last minute bar of the day.
                    day_trade_bars.get(&benchmark_sid)
                        .and_then(|bars| bars.last())
                        .map(|b| b.close)
                } else {
                    benchmark_price_map.get(&current_date).copied()
                };
                if let Some(close) = bm_close {
                    benchmark_curve.push(close);
                }

                // Record daily equity snapshot.
                let day_equity = portfolio.total_portfolio_value();
                equity_curve.push(day_equity);
                daily_dates.push(current_date.to_string());
            }

            current_date += chrono::Duration::days(1);
        } else {
            // ── DAILY LOOP ───────────────────────────────────────────────────
            let utc_time = date_to_datetime(current_date, 16, 0, 0);
            set_algorithm_time(&adapter, utc_time);

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
                    adapter.inner.lock().unwrap()
                        .securities.update_price(&bar.symbol, bar.close);
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

            let bars_map: HashMap<u64, lean_data::TradeBar> = slice.bars
                .iter()
                .map(|(&k, v)| (k, v.clone()))
                .collect();

            let fill_events = order_processor.process_orders(&bars_map, utc_time);
            all_order_events.extend(fill_events.iter().cloned());
            let ib_fee_model = InteractiveBrokersFeeModel::default();
            for event in &fill_events {
                if event.is_fill() {
                    if let Some(order) = order_processor.transaction_manager.get_order(event.order_id) {
                        let fee = ib_fee_model.get_order_fee(
                            &OrderFeeParameters::equity(&order, event.fill_price),
                        ).amount;
                        portfolio.apply_fill(
                            &order,
                            event.fill_price,
                            event.fill_quantity,
                            fee,
                        );
                    }

                    let sid = event.symbol.id.sid;
                    let fill_qty = event.fill_quantity;
                    if let Some((entry_time, entry_price, open_qty)) = open_positions.remove(&sid) {
                        let close_qty = open_qty.abs().min(fill_qty.abs());
                        completed_trades.push(Trade::new(
                            event.symbol.clone(),
                            entry_time,
                            event.utc_time,
                            entry_price,
                            event.fill_price,
                            close_qty,
                            rust_decimal_macros::dec!(0),
                        ));
                    } else {
                        open_positions.insert(sid, (event.utc_time, event.fill_price, fill_qty));
                    }
                }
                adapter.on_order_event(event);
            }

            // Build option chains before calling on_data.
            {
                let option_subs: Vec<Symbol> = {
                    adapter.inner.lock().unwrap().option_subscriptions.clone()
                };

                let mut chains_for_day: Vec<(String, OptionChain)> = Vec::new();
                for canonical in &option_subs {
                    let underlying_ticker = canonical.permtick.trim_start_matches('?');
                    let spot = {
                        adapter.inner.lock().unwrap()
                            .securities.all()
                            .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                            .map(|s| s.current_price())
                            .unwrap_or(Decimal::ZERO)
                    };

                    let chain = {
                        let ticker = underlying_ticker.to_uppercase();
                        let bars = tokio::task::spawn_blocking({
                            let provider = config.history_provider.clone();
                            let data_root = config.data_root.clone();
                            move || load_option_eod_bars(&data_root, &ticker, current_date, provider.as_ref())
                        }).await.unwrap_or_default();
                        if !bars.is_empty() {
                            build_option_chain_from_eod_bars(canonical, spot, current_date, &bars)
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
                alg.option_chains.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
            };

            // ── Custom data fetch (daily) ─────────────────────────────────
            let custom_subs: Vec<CustomDataSubscription> = {
                adapter.inner.lock().unwrap()
                    .custom_data_subscriptions.clone()
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
                    let source = config.custom_data_sources.iter()
                        .find(|s| s.name() == sub.source_type)
                        .cloned();
                    let data_root = config.data_root.clone();
                    let source_type = sub.source_type.clone();
                    let ticker = sub.ticker.clone();
                    let cfg = sub.config.clone();
                    let points = tokio::task::spawn_blocking(move || {
                        load_custom_data_points(&data_root, &source_type, &ticker, current_date, source.as_ref(), &cfg)
                    }).await.unwrap_or_default();
                    if !points.is_empty() {
                        custom_data_for_day.insert(sub.ticker.clone(), points);
                    }
                }
            }

            Python::with_gil(|py| {
                slice_proxy.update_option_chains(py, &chains_snapshot);
                slice_proxy.update_custom_data(py, &custom_data_for_day);
                adapter.on_data_proxy(py, &slice_proxy, &slice);
            });
            adapter.on_end_of_day(None);

            process_option_expirations(&mut adapter, current_date);

            let day_equity = portfolio.total_portfolio_value();
            equity_curve.push(day_equity);
            daily_dates.push(current_date.to_string());

            current_date += chrono::Duration::days(1);
        }
    }

    adapter.on_end_of_algorithm();

    let sim_elapsed = sim_start.elapsed();
    let pts_per_sec = if sim_elapsed.as_secs_f64() > 0.0 {
        trading_days as f64 / sim_elapsed.as_secs_f64()
    } else { f64::INFINITY };
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
    } else { 0.0 };

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

    let equity_curve_f64: Vec<f64> = equity_curve.iter().map(|v| {
        use rust_decimal::prelude::ToPrimitive;
        v.to_f64().unwrap_or(0.0)
    }).collect();

    // Collect charts from the strategy after the backtest completes.
    let charts = adapter.charts.lock()
        .map(|c| c.clone())
        .unwrap_or_default();

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
    provider: &dyn IHistoricalDataProvider,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<()> {
    for sub in subscriptions {
        let bar_path = resolver
            .trade_bar(&sub.symbol, sub.resolution, start)
            .to_path();

        let ticker = sub.symbol.permtick.to_lowercase();
        let market = sub.symbol.market().as_str().to_lowercase();
        let sec    = format!("{}", sub.symbol.security_type()).to_lowercase();
        let factor_path = resolver.data_root
            .join(&sec)
            .join(&market)
            .join("factor_files")
            .join(format!("{ticker}.parquet"));

        let factor_valid = factor_path.exists() && {
            let r = ParquetReader::new();
            r.read_factor_file(&factor_path).map_or(false, |rows| !rows.is_empty())
        };

        if bar_path.exists() && factor_valid {
            continue;
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
        let end_dt   = date_to_datetime(end, 23, 59, 59);

        if !bar_path.exists() {
            info!("No local data for {} — fetching from provider ({} → {})", sub.symbol.value, effective_start, end);
            let bars = provider
                .get_trade_bars(sub.symbol.clone(), sub.resolution, start_dt, end_dt)
                .await
                .map_err(|e| anyhow::anyhow!(
                    "historical provider failed for {}: {}", sub.symbol.value, e
                ))?;
            info!("Downloaded {} bars for {} and cached to disk", bars.len(), sub.symbol.value);
        }

        // Re-check after bar fetch — some providers write the factor file as a side-effect.
        if !factor_path.exists() {
            if let Some(ref fp) = factor_provider {
                info!("Factor file missing for {} — requesting from provider", sub.symbol.value);
                let fp = Arc::clone(fp);
                let request = lean_data_providers::HistoryRequest {
                    symbol:     sub.symbol.clone(),
                    resolution: lean_core::Resolution::Daily,
                    start:      start_dt,
                    end:        end_dt,
                    data_type:  lean_data_providers::DataType::FactorFile,
                };
                match tokio::task::spawn_blocking(move || fp.get_history(&request)).await {
                    Ok(Ok(_))  => info!("Factor file generated for {}", sub.symbol.value),
                    Ok(Err(e)) => warn!("Factor file generation failed for {}: {e}", sub.symbol.value),
                    Err(e)     => warn!("Factor file task panicked for {}: {e}", sub.symbol.value),
                }
            }
        }
    }
    Ok(())
}

// ─── helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap()))
}

fn day_key(date: NaiveDate) -> i64 {
    date.signed_duration_since(NaiveDate::from_ymd_opt(1, 1, 1).unwrap()).num_days()
}

/// Process option expirations for `current_date`.
///
/// Scans all open option positions for contracts expiring today, computes
/// intrinsic value, and handles exercise (long) or assignment (short).
fn process_option_expirations(adapter: &mut PyAlgorithmAdapter, current_date: NaiveDate) {
    // Collect expiring positions — we need to drop the lock before calling market_order.
    let expiring: Vec<lean_algorithm::qc_algorithm::OpenOptionPosition> = {
        let alg = adapter.inner.lock().unwrap();
        alg.option_positions
            .values()
            .filter(|pos| pos.expiry == current_date)
            .cloned()
            .collect()
    };

    if expiring.is_empty() {
        return;
    }

    for pos in expiring {
        // Get the spot price for the underlying.
        let spot = {
            let alg = adapter.inner.lock().unwrap();
            // Try to find the underlying security by permtick.
            let underlying_ticker = pos.symbol.underlying.as_ref()
                .map(|u| u.permtick.clone())
                .unwrap_or_default();
            let found: Option<Decimal> = alg.securities.all()
                .find(|s| s.symbol.permtick.eq_ignore_ascii_case(&underlying_ticker))
                .map(|s| s.current_price());
            found.unwrap_or(pos.strike) // Conservative fallback: use strike
        };

        let intrinsic = intrinsic_value(spot, pos.strike, pos.right);
        let exercised = intrinsic >= rust_decimal_macros::dec!(0.01);

        if exercised && pos.quantity > Decimal::ZERO {
            // Long position: auto-exercise
            let contracts = pos.quantity;
            let underlying_sym = pos.symbol.underlying.as_ref()
                .map(|u| *u.clone())
                .unwrap_or_else(|| pos.symbol.clone());

            // Shares from exercise: get_exercise_quantity uses LEAN sign convention.
            // For a long call: caller buys 100*qty shares, pays strike*100*qty.
            // For a long put: caller sells 100*qty shares, receives strike*100*qty.
            let exercise_shares = get_exercise_quantity(contracts, pos.right, 100);
            let shares_abs = exercise_shares.abs();

            {
                let mut alg = adapter.inner.lock().unwrap();
                // Settle the stock leg immediately at the strike price.
                // apply_exercise atomically creates/updates the holding and
                // adjusts cash — no market order queued, so the equity curve
                // on the expiration day is correct.
                alg.portfolio.apply_exercise(&underlying_sym, pos.strike, exercise_shares);
                alg.option_positions.remove(&pos.symbol.id.sid);
            }

            info!(
                "Option exercised: {} x{} K={} expiry={}",
                pos.symbol.value, contracts, pos.strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, contracts, true);
        } else if exercised && pos.quantity < Decimal::ZERO {
            // Short position: assignment
            let contracts = pos.quantity.abs();
            let underlying_sym = pos.symbol.underlying.as_ref()
                .map(|u| *u.clone())
                .unwrap_or_else(|| pos.symbol.clone());

            {
                let mut alg = adapter.inner.lock().unwrap();
                // Settle the stock leg immediately at the strike price.
                // For a short put: we must buy 100*qty shares at the strike → positive quantity.
                // For a short call: we must sell 100*qty shares at the strike → negative quantity.
                let shares = Decimal::from(100) * contracts;
                let exercise_qty = match pos.right {
                    OptionRight::Put  =>  shares,   // buy stock
                    OptionRight::Call => -shares,   // sell (or short) stock
                };
                alg.portfolio.apply_exercise(&underlying_sym, pos.strike, exercise_qty);
                alg.option_positions.remove(&pos.symbol.id.sid);
            }

            info!(
                "Option assigned: {} x{} K={} expiry={}",
                pos.symbol.value, pos.quantity, pos.strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, contracts, true);
        } else {
            // Expired worthless — premium already booked at trade open.
            let entry_price = pos.entry_price;
            {
                let mut alg = adapter.inner.lock().unwrap();
                alg.option_positions.remove(&pos.symbol.id.sid);
            }
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

/// Build a real option chain from ThetaData EOD rows for a single trading day.
///
/// Build an option chain directly from typed `OptionEodBar` rows.
///
/// Avoids the intermediate `V3OptionEod` representation — no string→date or
/// f64→Decimal round-trips.  Contracts expiring on or before `today` are skipped.
fn build_option_chain_from_eod_bars(
    canonical_sym: &Symbol,
    spot: Decimal,
    today: NaiveDate,
    bars: &[OptionEodBar],
) -> OptionChain {
    let mut chain = OptionChain::new(canonical_sym.clone(), spot);
    let underlying_sym: Symbol = canonical_sym.underlying.as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| canonical_sym.clone());
    let market = Market::usa();

    for bar in bars {
        if bar.expiration < today { continue; }  // include 0DTE (expiration == today)
        if bar.strike < Decimal::ONE { continue; }

        let right = match bar.right.to_ascii_lowercase().as_str() {
            "c" | "call" => OptionRight::Call,
            "p" | "put"  => OptionRight::Put,
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
        let last = if bar.close > Decimal::ZERO { bar.close } else { mid };

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
        return reader.read_option_eod_bars(&[cache_path]).unwrap_or_default();
    }

    // Cache miss — fetch from provider.
    let Some(provider) = provider else { return vec![]; };
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

/// Cache-first fetch for custom data points.
///
/// Cache layout: `{data_root}/custom/{source_type}/{ticker_lower}/{YYYYMMDD}.parquet`
/// One file per (source_type, ticker, date).  A cache hit is a single `path.exists()` check.
///
/// On cache miss, calls `source.get_source()` to get the URI, fetches via HTTP or reads a
/// local file, then calls `source.reader()` line-by-line to parse points.  Parsed points
/// are written to the Parquet cache before returning.
///
/// Called from inside `tokio::task::spawn_blocking` — all I/O is synchronous.
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
        return reader.read_custom_data_points(&cache_path).unwrap_or_default();
    }

    // Cache miss — need a source plugin to fetch.
    let Some(source) = source else { return vec![]; };

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
                        warn!("custom data fetch body error for {}/{} {}: {}", source_type, ticker, date, e);
                        return vec![];
                    }
                },
                Err(e) => {
                    warn!("custom data HTTP fetch failed for {}/{} {}: {}", source_type, ticker, date, e);
                    return vec![];
                }
            }
        }
        CustomDataTransport::LocalFile => {
            match std::fs::read_to_string(&data_source.uri) {
                Ok(content) => content,
                Err(e) => {
                    warn!("custom data file read failed for {}/{} {}: {}", source_type, ticker, date, e);
                    return vec![];
                }
            }
        }
    };

    // Parse content using the plugin's reader() method.
    let mut points: Vec<CustomDataPoint> = Vec::new();
    match data_source.format {
        CustomDataFormat::Csv | CustomDataFormat::JsonLines => {
            // Line-by-line: call reader() on each non-empty line.
            for line in raw_content.lines() {
                if line.trim().is_empty() { continue; }
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
                    warn!("custom data JSON parse error for {}/{} {}: {}", source_type, ticker, date, e);
                }
            }
        }
    }

    if points.is_empty() {
        return vec![];
    }

    // Write to Parquet cache.
    let writer = ParquetWriter::new(WriterConfig::default());
    if let Err(e) = writer.write_custom_data_points(&points, &cache_path) {
        warn!("failed to cache custom data for {}/{} {}: {}", source_type, ticker, date, e);
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
/// Mirrors LEAN C# `FactorFile.GetPriceFactor`: find the first row (ordered
/// ascending by date) whose date is >= bar_date.  That is the "current period"
/// factor row.  If no such row exists (bar is after the last factor entry),
/// return (1.0, 1.0) — identical to LEAN returning 1 for the most-recent row.
fn factor_for_entry(rows: &[FactorFileEntry], bar_date: NaiveDate) -> (f64, f64) {
    if rows.is_empty() { return (1.0, 1.0); }
    // rows are ordered ascending by date; find the first row where date >= bar_date
    let mut sorted: Vec<&FactorFileEntry> = rows.iter().collect();
    sorted.sort_by_key(|r| r.date);
    for r in &sorted {
        if r.date >= bar_date {
            return (r.price_factor, r.split_factor);
        }
    }
    // bar_date is after all factor file entries → no adjustment (same as C# LEAN)
    (1.0, 1.0)
}

fn apply_factor_row(mut bar: TradeBar, rows: &[FactorFileEntry], bar_date: NaiveDate) -> TradeBar {
    let (pf, sf) = factor_for_entry(rows, bar_date);
    let combined = pf * sf;
    if (combined - 1.0).abs() < 1e-9 { return bar; } // fast-path: no adjustment

    let scale = Decimal::from_f64(combined).unwrap_or(Decimal::ONE);
    bar.open   *= scale;
    bar.high   *= scale;
    bar.low    *= scale;
    bar.close  *= scale;
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
    use lean_storage::schema::FactorFileEntry;
    use chrono::NaiveDate;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn entry(y: i32, m: u32, day: u32, pf: f64) -> FactorFileEntry {
        FactorFileEntry { date: d(y, m, day), price_factor: pf, split_factor: 1.0, reference_price: 0.0 }
    }

    /// SPY-like factor file: entries starting 2021-03-25.
    fn spy_rows() -> Vec<FactorFileEntry> {
        vec![
            entry(2021, 3, 25, 0.9339743),
            entry(2021, 6, 17, 0.9339743),
            entry(2021, 9, 16, 0.9370296),
            entry(2021, 12, 16, 0.9400318),
            entry(2022, 3, 17, 0.9433413),
            entry(2026, 4,  9, 1.0),
        ]
    }

    /// Bar before the first factor file entry → no adjustment (factor = 1.0).
    /// Matches LEAN: no preceding row → returns 1.
    #[test]
    fn test_before_first_entry_returns_one() {
        let rows = spy_rows();
        assert_eq!(factor_for_entry(&rows, d(2020, 10, 16)), (1.0, 1.0),
            "bars before the factor file must get factor=1.0 (no adjustment)");
    }

    /// Bar exactly on the first entry date → still returns 1.0.
    /// LEAN's condition is strict `<`, so the row on 2021-03-25 does NOT apply
    /// to a bar also dated 2021-03-25.
    #[test]
    fn test_on_first_entry_date_returns_one() {
        let rows = spy_rows();
        assert_eq!(factor_for_entry(&rows, d(2021, 3, 25)), (1.0, 1.0),
            "bar on the first entry date must still return 1.0 (strict <)");
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
        assert!((pf - 0.9370296).abs() < 1e-7,
            "should pick the Sep-16 entry, not the Dec-16 one");
    }

    /// Bar exactly on a non-first entry date → picks the preceding entry.
    #[test]
    fn test_on_middle_entry_date_picks_previous() {
        let rows = spy_rows();
        // On 2021-09-16 exactly → the Sep-16 row itself has date = bar_date,
        // so strict < excludes it; we get the Jun-17 entry (0.9339743).
        let (pf, _) = factor_for_entry(&rows, d(2021, 9, 16));
        assert!((pf - 0.9339743).abs() < 1e-7,
            "bar ON an entry date picks the entry before it (strict <)");
    }

    /// Bar after the last entry (2026-04-09) → picks the 2026-04-09 entry (factor=1.0).
    #[test]
    fn test_after_last_entry_picks_last() {
        let rows = spy_rows();
        let (pf, _) = factor_for_entry(&rows, d(2026, 4, 10));
        assert!((pf - 1.0).abs() < 1e-9,
            "bars after the last entry get factor=1.0");
    }

    /// Jan 4, 2022 must use the 2021-12-16 entry (0.9400318) — matches the
    /// observed LEAN C# value from the real SMA-crossover backtest log.
    #[test]
    fn test_jan_2022_matches_lean_observed() {
        let rows = spy_rows();
        let (pf, _) = factor_for_entry(&rows, d(2022, 1, 4));
        assert!((pf - 0.9400318).abs() < 1e-7,
            "2022-01-04 must use the 2021-12-16 factor (0.9400318)");
    }

    /// Empty rows → always 1.0.
    #[test]
    fn test_empty_rows() {
        assert_eq!(factor_for_entry(&[], d(2020, 1, 1)), (1.0, 1.0));
    }
}

// ─── benchmark unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lean_algorithm::qc_algorithm::QcAlgorithm;
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
        let returns: Vec<Decimal> = prices.windows(2)
            .map(|w| (w[1] - w[0]) / w[0])
            .collect();

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
        use lean_statistics::PortfolioStatistics;
        use crate::charting::ChartCollection;

        let stats = PortfolioStatistics::compute(
            &[dec!(100_000), dec!(101_000)],
            &[dec!(400), dec!(402)],
            &[],
            1,
            dec!(100_000),
            dec!(0),
        );
        let result = BacktestResult {
            trading_days:            1,
            final_value:             101_000.0,
            total_return:            0.01,
            starting_cash:           100_000.0,
            equity_curve:            vec![100_000.0, 101_000.0],
            daily_dates:             vec!["2024-01-02".to_string(), "2024-01-03".to_string()],
            statistics:              stats,
            charts:                  ChartCollection::default(),
            order_events:            vec![],
            succeeded_data_requests: vec![],
            failed_data_requests:    vec![],
            backtest_id:             1_700_000_000,
            benchmark_symbol:        "QQQ".to_string(),
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
        use std::sync::Arc;
        use lean_data::SubscriptionDataConfig;
        use lean_core::{Resolution, Market, Symbol};

        let market = Market::usa();
        let spy = Symbol::create_equity("SPY", &market);
        let qqq = Symbol::create_equity("QQQ", &market);

        let cfg_spy = Arc::new(SubscriptionDataConfig::new_equity(spy.clone(), Resolution::Daily));
        let subs = vec![cfg_spy];

        // SPY is in subs → benchmark_in_subs = true
        let benchmark_ticker = "SPY";
        let in_subs = subs.iter().any(|s| s.symbol.permtick.eq_ignore_ascii_case(benchmark_ticker));
        assert!(in_subs);

        // QQQ is NOT in subs → benchmark_in_subs = false
        let benchmark_ticker2 = "QQQ";
        let in_subs2 = subs.iter().any(|s| s.symbol.permtick.eq_ignore_ascii_case(benchmark_ticker2));
        assert!(!in_subs2);
    }
}
