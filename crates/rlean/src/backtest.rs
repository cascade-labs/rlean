use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::cli::RunArgs;
use crate::config;
use crate::container_runtime::{self, BacktestRunSpec};
use crate::runtime::{
    backtest_progress_bar, connect_verglas, ensure_python_baseline_packages,
    historical_data_provider, parse_algorithm_parameters_for_strategy, strategy_name_from_path,
    validate_strategy_path,
};

pub(crate) async fn run(mut args: RunArgs) -> Result<()> {
    // If the user passed a directory, look for main.py inside it.
    if args.strategy.is_dir() {
        let candidate = args.strategy.join("main.py");
        if candidate.exists() {
            args.strategy = candidate;
        } else {
            bail!(
                "'{}' is a directory but contains no main.py. \
                 Pass the strategy file directly or run `rlean create-project` to scaffold one.",
                args.strategy.display()
            );
        }
    }

    validate_strategy_path(&args.strategy)?;

    if !args.native {
        return run_containerized(args);
    }

    let global_config = config::GlobalConfig::load()?;
    let verglas = connect_verglas(&global_config).await?;
    let (historical_provider, dropped_cache_writes) =
        historical_data_provider(&args, verglas.clone()).await?;

    run_strategy_backtest(args, historical_provider, dropped_cache_writes, verglas).await
}

fn run_containerized(args: RunArgs) -> Result<()> {
    let global = config::GlobalConfig::load()?;
    let image = container_runtime::default_image();
    let engine_args = container_runtime::runtime_engine_args(
        args.data_provider_historical.as_deref(),
        args.live_data_feed.as_deref(),
        args.brokerage.as_deref(),
        args.brokerage_url.as_deref(),
        args.brokerage_account.as_deref(),
        args.start_date.as_deref(),
        args.end_date.as_deref(),
        &args.parameters,
        args.verbose,
        args.live_limits.live_max_slices,
        args.live_limits.live_max_runtime_seconds,
    );
    let code = container_runtime::run_backtest_container(BacktestRunSpec {
        image: &image,
        pull: args.pull,
        strategy_file: &args.strategy,
        engine_args,
        global: &global,
    })?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

async fn run_strategy_backtest(
    args: RunArgs,
    historical_provider: Arc<dyn rlean_data_providers::HistoricalDataProvider>,
    dropped_cache_writes: rlean_data_providers::DroppedCacheWrites,
    verglas: verglas_sdk::Client,
) -> Result<()> {
    use rlean_python_runtime::AlgorithmImports;

    let ext = args
        .strategy
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    if ext == "py" {
        ensure_python_baseline_packages()?;
        pyo3::append_to_inittab!(AlgorithmImports);
        pyo3::Python::initialize();
    }

    let parse_date = |s: &str| -> Result<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid date '{}', expected YYYY-MM-DD", s))
    };
    let start_date_override = args.start_date.as_deref().map(parse_date).transpose()?;
    let end_date_override = args.end_date.as_deref().map(parse_date).transpose()?;
    let parameters = parse_algorithm_parameters_for_strategy(&args.strategy, &args.parameters)?;

    let strategy_name = strategy_name_from_path(&args.strategy);
    let run_id = format!("bt-{}", uuid::Uuid::new_v4());
    let run_catalog = crate::run_catalog::RunCatalog::connect(
        verglas,
        run_id,
        strategy_name.clone(),
        &parameters,
    )
    .await?;
    run_catalog.record_started().await?;

    // Stream progress and fills through a bounded channel. Backtests can
    // advance much faster than an Iceberg commit; collecting 32 updates keeps
    // result persistence from contending with historical queries while still
    // publishing multiple progress snapshots during a long run. Channel close
    // always flushes the final partial batch.
    let (stream_tx, mut stream_rx) = tokio::sync::mpsc::channel(256);
    let stream_catalog = run_catalog.clone();
    let stream_task = tokio::spawn(async move {
        let mut sequence = 0_u64;
        let mut updates = Vec::with_capacity(32);
        while let Some(update) = stream_rx.recv().await {
            updates.push(update);
            if updates.len() < 32 {
                continue;
            }
            stream_catalog
                .record_stream_updates(std::mem::take(&mut updates), sequence)
                .await?;
            sequence = sequence.wrapping_add(1);
        }
        if !updates.is_empty() {
            stream_catalog
                .record_stream_updates(updates, sequence)
                .await?;
        }
        Ok::<_, anyhow::Error>(())
    });

    let progress_bar = backtest_progress_bar();
    progress_bar.set_position(0);
    progress_bar.set_message("initializing | value $100000.00 | 0 trading days");
    progress_bar.tick();
    let progress = {
        let progress_bar = progress_bar.clone();
        let last_drawn_percent = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let last_drawn_percent_for_progress = last_drawn_percent.clone();
        Arc::new(move |progress: rlean_engine::BacktestProgress| {
            let total_days = (progress.end_date - progress.start_date).num_days().max(1) as u64;
            let elapsed_days = (progress.current_date - progress.start_date)
                .num_days()
                .clamp(0, total_days as i64) as u64;
            let pct = elapsed_days.saturating_mul(100).saturating_div(total_days);
            let pct = pct.min(100);
            progress_bar.set_message(format!(
                "{} | value ${:.2} | {} trading days",
                progress.current_date, progress.portfolio_value, progress.trading_days
            ));
            if last_drawn_percent_for_progress.swap(pct, std::sync::atomic::Ordering::Relaxed)
                != pct
            {
                progress_bar.set_position(pct);
            } else {
                progress_bar.tick();
            }
        })
    };
    let data_feed_options = rlean_engine::data_feed::DataFeedOptions::default();
    let config = rlean_engine::BacktestRunConfig {
        historical_provider: historical_provider.clone(),
        _compression_level: 3,
        start_date_override,
        end_date_override,
        parameters,
        data_feed_options,
        progress: Some(progress),
        stream_updates: Some(stream_tx),
    };

    let runtime_context =
        rlean_engine::AlgorithmRuntimeContext::new(historical_provider, config.parameters.clone());

    let bridge: Box<dyn rlean_sdk::AlgorithmBridge> = match ext {
        "py" => {
            let strategy_path = args.strategy.clone();
            let state = Arc::new(std::sync::Mutex::new(
                rlean_algorithm::qc_algorithm::QcAlgorithm::new(
                    "Algorithm",
                    rust_decimal::Decimal::new(100000, 0),
                ),
            ));
            let context =
                rlean_sdk::algorithm::AlgorithmConstructionContext::new_with_runtime_services(
                    state,
                    Arc::new(runtime_context.clone()),
                );
            let adapter =
                rlean_python_runtime::load_strategy_bridge_with_context(&strategy_path, context)?;
            Box::new(adapter)
        }
        other => bail!("Unknown strategy extension '.{}'. Expected .py", other),
    };

    let run_result =
        rlean_engine::runner::backtest::run_backtest_with_runtime(bridge, config, runtime_context)
            .await;
    if let Err(progress_error) = stream_task
        .await
        .context("join Verglas run stream writer")?
    {
        let error = anyhow::anyhow!("persist backtest progress: {progress_error:#}");
        let _ = run_catalog.record_failed(&error).await;
        return Err(error);
    }

    let results = match run_result {
        Ok(results) => results,
        Err(error) if rlean_sdk::interrupt::is_interrupted_error(&error) => {
            std::process::exit(130);
        }
        Err(error) => {
            if let Err(catalog_error) = run_catalog.record_failed(&error).await {
                return Err(error).context(format!(
                    "also failed to persist failed run state: {catalog_error}"
                ));
            }
            return Err(error);
        }
    };

    progress_bar.finish_with_message(format!(
        "{} | value ${:.2} | {} trading days",
        results.end_date, results.final_value, results.trading_days
    ));
    results.print_summary();
    // Persistence that was dropped to keep the run alive is reported once here
    // so a run that lost cache or telemetry writes does not look clean.
    dropped_cache_writes.log_summary();
    run_catalog.log_dropped_appends_summary();
    run_catalog.record_completed(&results).await?;
    println!("Results: verglas://rlean/runs/{}", run_catalog.run_id());

    Ok(())
}
