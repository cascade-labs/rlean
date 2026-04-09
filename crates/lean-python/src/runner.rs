/// Standalone Python strategy runner.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
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
use lean_data::{IHistoricalDataProvider, Slice, SubscriptionDataConfig, TradeBar};
use lean_options::{
    BlackScholesPriceModel, IOptionPriceModel, OptionChain, OptionContract, OptionContractData,
};
use lean_options::payoff::{intrinsic_value, is_auto_exercised, get_exercise_quantity};
use lean_orders::{
    fill_model::ImmediateFillModel,
    order_processor::OrderProcessor,
    slippage::NullSlippageModel,
};
use lean_data_providers::{ThetaDataClient, V3OptionEod, normalize_right as td_normalize_right, normalize_expiration as td_normalize_exp};
use lean_statistics::{PortfolioStatistics, Trade};
use lean_storage::{DataCache, FactorFileEntry, ParquetReader, PathResolver, QueryParams};

use crate::charting::ChartCollection;
use crate::py_adapter::{PyAlgorithmAdapter, set_algorithm_time};
use crate::py_qc_algorithm::PyQcAlgorithm;

pub struct RunConfig {
    pub data_root: PathBuf,
    pub _compression_level: i32,
    /// If set, missing price data is fetched from this provider before the backtest loop.
    pub historical_provider: Option<Arc<dyn IHistoricalDataProvider>>,
    /// ThetaData API key for fetching real option EOD chains.
    pub thetadata_api_key: Option<String>,
    /// Data root for the ThetaData Parquet store.  Option EOD bars are cached at
    /// `{thetadata_data_root}/option/usa/daily/{underlying}/{underlying}_eod.parquet`.
    /// Defaults to `data_root` when not set.
    pub thetadata_data_root: Option<PathBuf>,
    /// Requests per second for ThetaData option chain fetches (default 4.0).
    pub thetadata_rps: f64,
    /// Max concurrent ThetaData requests (default 4).
    pub thetadata_concurrent: usize,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            data_root: PathBuf::from("data"),
            _compression_level: 3,
            historical_provider: None,
            thetadata_api_key: None,
            thetadata_data_root: None,
            thetadata_rps: 4.0,
            thetadata_concurrent: 4,
        }
    }
}

pub struct BacktestResult {
    pub trading_days:  i64,
    pub final_value:   f64,
    pub total_return:  f64,
    pub starting_cash: f64,
    /// Daily portfolio values (one per trading day, in order).
    pub equity_curve:  Vec<f64>,
    /// ISO date strings matching equity_curve.
    pub daily_dates:   Vec<String>,
    /// Full statistics computed at the end of the backtest.
    pub statistics:    PortfolioStatistics,
    /// Custom strategy charts plotted via self.plot().
    pub charts:        ChartCollection,
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
        println!("╠══════════════════════════════════════════════════════╣");
        row("Equity Round-Trips",     &s.total_trades.to_string());
        row("Win Rate",               &format!("{:.1}%", s.win_rate.to_f64().unwrap_or(0.0) * 100.0));
        row("Loss Rate",              &format!("{:.1}%", s.loss_rate.to_f64().unwrap_or(0.0) * 100.0));
        row("Profit/Loss Ratio",      &format!("{:.2}", s.profit_loss_ratio.to_f64().unwrap_or(0.0)));
        row("Avg Win",                &format!("${:.2}", s.average_win_rate.to_f64().unwrap_or(0.0)));
        row("Avg Loss",               &format!("${:.2}", s.average_loss_rate.to_f64().unwrap_or(0.0)));
        row("Largest Win",            &format!("${:.2}", s.largest_win.to_f64().unwrap_or(0.0)));
        row("Largest Loss",           &format!("${:.2}", s.largest_loss.to_f64().unwrap_or(0.0)));
        row("Max Consecutive Wins",   &s.max_consecutive_wins.to_string());
        row("Max Consecutive Losses", &s.max_consecutive_losses.to_string());
        row("Avg Trade Duration",     &format!("{:.1} days", s.average_trade_duration_days.to_f64().unwrap_or(0.0)));
        row("Expectancy",             &format!("${:.2}", s.expectancy.to_f64().unwrap_or(0.0)));
        row("Total Net Profit",       &format!("${:.2}", s.total_net_profit.to_f64().unwrap_or(0.0)));
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
#[tokio::main]
pub async fn run_strategy(strategy_path: &Path, config: RunConfig) -> Result<BacktestResult> {
    let mut adapter = Python::with_gil(|py| load_strategy(py, strategy_path))?;

    // ── initialize ──────────────────────────────────────────────────────────
    adapter.initialize().context("strategy initialize() failed")?;

    let start_date = adapter.start_date().date_utc();
    let end_date   = adapter.end_date().date_utc();

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
            &subscriptions,
            start_date,
            end_date,
            &resolver,
        ).await?;
    }

    // ── factor files: load from disk ─────────────────────────────────────────
    // Factor files are Parquet; key = symbol SID → rows sorted newest first.
    // Factor files are written to disk by data providers (e.g. Polygon) that
    // support corporate actions.  When using a stacked provider list such as
    // "thetadata,polygon", the Polygon provider in the stack will generate the
    // factor file as a side-effect of fetching bars.
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
                    "Factor file missing for {} — bars will not be adjusted. \
                     Add 'polygon' to --data-provider-historical to generate factor files.",
                    sub.symbol.value
                );
            }
        }
    }

    // ── ThetaData client for on-demand option EOD chains ─────────────────────
    // Chains are fetched per-date during the backtest loop and stored in the
    // structured Parquet data store under data_root.
    let theta_client: Option<ThetaDataClient> = {
        let has_options = !adapter.inner.lock().unwrap().option_subscriptions.is_empty();
        if has_options {
            if let Some(ref api_key) = config.thetadata_api_key {
                let data_root = config.thetadata_data_root.clone()
                    .unwrap_or_else(|| config.data_root.clone());
                Some(ThetaDataClient::new(
                    Some(api_key.clone()),
                    config.thetadata_rps,
                    config.thetadata_concurrent,
                    &data_root,
                ))
            } else {
                warn!("Option subscriptions present but no --thetadata-api-key — using synthetic chains");
                None
            }
        } else {
            None
        }
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

    // ── date loop ───────────────────────────────────────────────────────────
    let mut current_date = start_date;
    let mut trading_days = 0i64;
    let mut equity_curve: Vec<Decimal> = Vec::new();
    let mut daily_dates: Vec<String> = Vec::new();

    // Benchmark curve: track the close price of the SPY subscription (or first
    // equity subscription) in parallel with the equity curve.
    let benchmark_sid: Option<u64> = subscriptions.iter()
        .find(|s| s.symbol.permtick.eq_ignore_ascii_case("SPY"))
        .or_else(|| subscriptions.first())
        .map(|s| s.symbol.id.sid);
    let mut benchmark_curve: Vec<Decimal> = Vec::new();

    // Trade tracking: open_positions maps symbol SID → (entry_time, entry_price, quantity).
    // When a fill closes a position we emit a completed Trade.
    let mut open_positions: HashMap<u64, (DateTime, Decimal, Decimal)> = HashMap::new();
    let mut completed_trades: Vec<Trade> = Vec::new();

    info!("Backtest: {} → {}", start_date, end_date);

    while current_date <= end_date {
        let utc_time = date_to_datetime(current_date, 16, 0, 0);
        set_algorithm_time(&adapter, utc_time);

        let mut slice = Slice::new(utc_time);
        for sub in &subscriptions {
            let sid      = sub.symbol.id.sid;
            let day_key  = day_key(current_date);
            let path     = resolver.trade_bar(&sub.symbol, sub.resolution, current_date).to_path();

            if path.exists() {
                let bars = if let Some(cached) = cache.get_bars(sid, day_key) {
                    cached.as_ref().clone()
                } else {
                    let day_start = date_to_datetime(current_date, 0, 0, 0);
                    let day_end   = date_to_datetime(current_date, 23, 59, 59);
                    let params    = QueryParams::new().with_time_range(day_start, day_end);
                    let loaded    = reader
                        .read_trade_bars(&[path], sub.symbol.clone(), &params)
                        .await
                        .unwrap_or_default();
                    cache.insert_bars(sid, day_key, loaded.clone());
                    loaded
                };

                for bar in bars {
                    // Apply factor-file adjustments when raw bars are on disk.
                    let bar = if let Some(rows) = factor_map.get(&sid) {
                        apply_factor_row(bar, rows, current_date)
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

        if !slice.has_data {
            current_date += chrono::Duration::days(1);
            continue;
        }

        // Record benchmark close for this trading day.
        if let Some(bsid) = benchmark_sid {
            if let Some(bar) = slice.bars.get(&bsid) {
                benchmark_curve.push(bar.close);
            }
        }

        trading_days += 1;

        let bars_map: HashMap<u64, lean_data::TradeBar> = slice.bars
            .iter()
            .map(|(&k, v)| (k, v.clone()))
            .collect();

        let fill_events = order_processor.process_orders(&bars_map, utc_time);
        for event in &fill_events {
            if event.is_fill() {
                if let Some(order) = order_processor.transaction_manager.get_order(event.order_id) {
                    portfolio.apply_fill(
                        &order,
                        event.fill_price,
                        event.fill_quantity,
                        rust_decimal_macros::dec!(0),
                    );
                }

                // Build round-trip trades from fills.
                let sid = event.symbol.id.sid;
                let fill_qty = event.fill_quantity;
                if let Some((entry_time, entry_price, open_qty)) = open_positions.remove(&sid) {
                    // Closing fill: emit a completed trade.
                    // Quantity for P&L purposes is the absolute size being closed.
                    let close_qty = open_qty.abs().min(fill_qty.abs());
                    completed_trades.push(Trade::new(
                        event.symbol.clone(),
                        entry_time,
                        event.utc_time,
                        entry_price,
                        event.fill_price,
                        close_qty,
                        rust_decimal_macros::dec!(0), // fees not tracked here
                    ));
                } else {
                    // Opening fill: record the entry.
                    open_positions.insert(sid, (event.utc_time, event.fill_price, fill_qty));
                }
            }
            adapter.on_order_event(event);
        }

        // Build option chains for subscriptions before calling on_data.
        // Real ThetaData EOD bars are fetched per-date and disk-cached.
        // Fall back to synthetic Black-Scholes chains when no API key is configured.
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

                let chain = if let Some(ref client) = theta_client {
                    let ticker = underlying_ticker.to_uppercase();
                    match client.get_option_eod_for_date(&ticker, current_date).await {
                        Ok(rows) if !rows.is_empty() => {
                            build_option_chain_from_eod(canonical, spot, current_date, &rows)
                        }
                        Ok(_) => {
                            // No data for this date (non-trading day or gap) — skip silently.
                            generate_option_chain(canonical, spot, current_date)
                        }
                        Err(e) => {
                            warn!("ThetaData option EOD fetch failed for {underlying_ticker} on {current_date}: {e}");
                            generate_option_chain(canonical, spot, current_date)
                        }
                    }
                } else {
                    generate_option_chain(canonical, spot, current_date)
                };
                chains_for_day.push((canonical.permtick.clone(), chain));
            }

            let mut alg = adapter.inner.lock().unwrap();
            for (permtick, chain) in chains_for_day {
                alg.option_chains.insert(permtick, chain);
            }
        }

        adapter.on_data(&slice);
        adapter.on_end_of_day(None);

        // Process option expirations for today.
        process_option_expirations(&mut adapter, current_date);

        // Record daily equity snapshot.
        let day_equity = portfolio.total_portfolio_value();
        equity_curve.push(day_equity);
        daily_dates.push(current_date.to_string());

        current_date += chrono::Duration::days(1);
    }

    adapter.on_end_of_algorithm();

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
        equity_curve: equity_curve_f64,
        daily_dates,
        statistics,
        charts,
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
    subscriptions: &[Arc<SubscriptionDataConfig>],
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<()> {
    for sub in subscriptions {
        let check_path = resolver
            .trade_bar(&sub.symbol, sub.resolution, start)
            .to_path();

        if check_path.exists() {
            continue; // already have local data
        }

        info!(
            "No local data for {} — fetching from provider ({} → {})",
            sub.symbol.value, start, end
        );

        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt   = date_to_datetime(end, 23, 59, 59);

        let bars = provider
            .get_trade_bars(sub.symbol.clone(), sub.resolution, start_dt, end_dt)
            .await
            .map_err(|e| anyhow::anyhow!(
                "historical provider failed for {}: {}", sub.symbol.value, e
            ))?;

        info!(
            "Downloaded {} bars for {} and cached to disk",
            bars.len(), sub.symbol.value
        );
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
                // Deliver/receive underlying shares at strike price.
                alg.market_order(&underlying_sym, exercise_shares);
                // Debit/credit strike price cash.
                match pos.right {
                    OptionRight::Call => {
                        // Buy shares: pay strike * shares
                        *alg.portfolio.cash.write() -= pos.strike * shares_abs;
                    }
                    OptionRight::Put => {
                        // Sell shares: receive strike * shares
                        *alg.portfolio.cash.write() += pos.strike * shares_abs;
                    }
                }
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
                match pos.right {
                    OptionRight::Put => {
                        // Short put assigned: must buy underlying at strike
                        let shares = Decimal::from(100) * contracts;
                        alg.market_order(&underlying_sym, shares);
                        *alg.portfolio.cash.write() -= pos.strike * shares;
                    }
                    OptionRight::Call => {
                        // Short call assigned: must sell underlying at strike
                        let shares = Decimal::from(100) * contracts;
                        alg.market_order(&underlying_sym, -shares);
                        *alg.portfolio.cash.write() += pos.strike * shares;
                    }
                }
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
            {
                let mut alg = adapter.inner.lock().unwrap();
                alg.option_positions.remove(&pos.symbol.id.sid);
            }
            info!(
                "Option expired worthless: {} x{} K={} expiry={}",
                pos.symbol.value, pos.quantity, pos.strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, pos.quantity.abs(), false);
        }
    }
}

/// Build a real option chain from ThetaData EOD rows for a single trading day.
///
/// Strikes are in milli-dollars (e.g. 450000 = $450.00). Expirations are
/// `yyyyMMdd` or `yyyy-MM-dd`. Contracts expiring on or before `today` are skipped.
fn build_option_chain_from_eod(
    canonical_sym: &Symbol,
    spot: Decimal,
    today: NaiveDate,
    rows: &[V3OptionEod],
) -> OptionChain {
    let mut chain = OptionChain::new(canonical_sym.clone(), spot);

    let underlying_sym: Symbol = canonical_sym.underlying.as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| canonical_sym.clone());
    let market = Market::usa();

    for row in rows {
        // ThetaData V3 option/history/eod returns strikes in dollars.
        let strike = match Decimal::from_f64(row.strike) {
            Some(s) if s >= Decimal::ONE => s,
            _ => continue,
        };

        // Normalize and parse expiration.
        let exp_str = td_normalize_exp(&row.expiration);
        let expiry = match NaiveDate::parse_from_str(&exp_str, "%Y%m%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        if expiry <= today { continue; }

        // Normalize right.
        let right = match td_normalize_right(&row.right) {
            "c" => OptionRight::Call,
            "p" => OptionRight::Put,
            _ => continue,
        };

        let sym = Symbol::create_option_osi(
            underlying_sym.clone(),
            strike,
            expiry,
            right,
            OptionStyle::American,
            &market,
        );

        let mut contract = OptionContract::new(sym);
        let bid   = Decimal::from_f64(row.bid_price).unwrap_or(Decimal::ZERO);
        let ask   = Decimal::from_f64(row.ask_price).unwrap_or(Decimal::ZERO);
        let close = Decimal::from_f64(row.close).unwrap_or(Decimal::ZERO);
        let mid   = if bid > Decimal::ZERO && ask > Decimal::ZERO {
            (bid + ask) / rust_decimal_macros::dec!(2)
        } else {
            close
        };
        let last  = if close > Decimal::ZERO { close } else { mid };

        contract.data = OptionContractData {
            underlying_last_price: spot,
            bid_price: bid,
            ask_price: ask,
            last_price: last,
            volume: row.volume as i64,
            bid_size: row.bid_size as i64,
            ask_size: row.ask_size as i64,
            ..Default::default()
        };

        chain.add_contract(contract);
    }

    chain
}

/// Generate a synthetic option chain for `canonical_sym` centred on `spot`.
///
/// Produces call and put contracts for:
/// - Strikes: ATM ± 5%, 10%, 15%, 20% rounded to nearest $5.
/// - Expirations: 3rd Friday of each of the next 3 calendar months.
/// - Greeks/price via Black-Scholes at 20% IV.
fn generate_option_chain(
    canonical_sym: &Symbol,
    spot: Decimal,
    today: NaiveDate,
) -> OptionChain {
    let mut chain = OptionChain::new(canonical_sym.clone(), spot);

    if spot.is_zero() {
        return chain;
    }

    // Derive the underlying Symbol from the canonical's underlying field.
    let underlying_sym: Symbol = canonical_sym.underlying.as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| canonical_sym.clone());

    // Compute target strikes: ATM ± 5,10,15,20% rounded to nearest $5.
    let spot_f64 = spot.to_f64().unwrap_or(1.0);
    let pcts = [-0.20, -0.15, -0.10, -0.05, 0.0, 0.05, 0.10, 0.15, 0.20];
    let strikes: Vec<Decimal> = pcts.iter().map(|&pct| {
        let raw = spot_f64 * (1.0 + pct);
        let rounded = (raw / 5.0).round() * 5.0;
        Decimal::from_f64(rounded.max(1.0)).unwrap_or(spot)
    }).collect();

    // Compute expirations: 3rd Friday of each of the next 3 months.
    let expirations: Vec<NaiveDate> = (1i32..=3).filter_map(|months_ahead| {
        let total_months = today.month() as i32 + months_ahead;
        let year = today.year() + (total_months - 1) / 12;
        let month = ((total_months - 1) % 12 + 1) as u32;
        third_friday(year, month)
    }).collect();

    let bs = BlackScholesPriceModel;
    let market = Market::usa();

    for &expiry in &expirations {
        let t_days = (expiry - today).num_days();
        if t_days <= 0 { continue; }

        for &strike in &strikes {
            for &right in &[OptionRight::Call, OptionRight::Put] {
                let sym = Symbol::create_option_osi(
                    underlying_sym.clone(),
                    strike,
                    expiry,
                    right,
                    OptionStyle::American,
                    &market,
                );

                let mut contract = OptionContract::new(sym);
                let iv = rust_decimal_macros::dec!(0.20);
                contract.data = OptionContractData {
                    underlying_last_price: spot,
                    implied_volatility: iv,
                    ..Default::default()
                };

                // Price via Black-Scholes.
                let result = bs.evaluate(&contract, 0.05, 0.0);
                contract.data.theoretical_price = result.theoretical_price;
                contract.data.bid_price = (result.theoretical_price
                    - rust_decimal_macros::dec!(0.05)).max(Decimal::ZERO);
                contract.data.ask_price = result.theoretical_price
                    + rust_decimal_macros::dec!(0.05);
                contract.data.last_price = result.theoretical_price;
                contract.data.greeks = result.greeks;

                chain.add_contract(contract);
            }
        }
    }

    chain
}

/// Return the 3rd Friday of the given year and month, or None if invalid.
fn third_friday(year: i32, month: u32) -> Option<NaiveDate> {
    use chrono::Weekday;
    // Find the first day of the month.
    let first = NaiveDate::from_ymd_opt(year, month, 1)?;
    // Find the first Friday.
    let first_friday_offset = {
        let wd = first.weekday().num_days_from_monday(); // Mon=0 .. Sun=6
        // Friday = 4
        (4 + 7 - wd) % 7
    };
    // Third Friday = first Friday + 14 days.
    first.checked_add_signed(chrono::Duration::days(
        first_friday_offset as i64 + 14,
    ))
}

/// Apply a factor-file adjustment to a raw bar.
///
/// Looks up `(price_factor, split_factor)` for `bar_date` and scales
/// all OHLCV fields by `price_factor * split_factor`.  Volume is scaled
/// inversely (more shares at lower prices after a split).
fn factor_for_entry(rows: &[FactorFileEntry], bar_date: NaiveDate) -> (f64, f64) {
    if rows.is_empty() { return (1.0, 1.0); }
    let mut best: Option<&FactorFileEntry> = None;
    for r in rows {
        if r.date > bar_date {
            match best {
                None => best = Some(r),
                Some(b) if r.date < b.date => best = Some(r),
                _ => {}
            }
        }
    }
    match best {
        Some(r) => (r.price_factor, r.split_factor),
        None    => (1.0, 1.0),
    }
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
