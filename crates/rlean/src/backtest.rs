use std::sync::Arc;

use anyhow::{bail, Context, Result};
use rlean_data_sidecar::DataSidecarClient;

use crate::cli::RunArgs;
use crate::config;
use crate::runtime::{
    backtest_progress_bar, connect_data_sidecar, ensure_python_baseline_packages,
    parse_algorithm_parameters_for_strategy, strategy_name_from_path, validate_strategy_path,
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

    let global_config = config::GlobalConfig::load()?;
    let data_sidecar = connect_data_sidecar(&args, &global_config)
        .await?
        .ok_or_else(|| anyhow::anyhow!("backtests require --data-sidecar"))?;

    run_strategy_backtest(args, data_sidecar).await
}

async fn run_strategy_backtest(args: RunArgs, data_sidecar: Arc<DataSidecarClient>) -> Result<()> {
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
    let run_catalog =
        crate::run_catalog::RunCatalog::connect(run_id, strategy_name.clone(), &parameters).await?;
    run_catalog.record_started().await?;

    // Progress is committed in bounded Arrow batches on a background task so
    // catalog latency never blocks the deterministic engine loop.
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::unbounded_channel();
    let progress_catalog = run_catalog.clone();
    let progress_task = tokio::spawn(async move {
        let mut buffered = Vec::with_capacity(32);
        while let Some(row) = progress_rx.recv().await {
            buffered.push(row);
            if buffered.len() == 32 {
                progress_catalog
                    .record_progress(std::mem::take(&mut buffered))
                    .await?;
            }
        }
        progress_catalog.record_progress(buffered).await
    });

    let progress_bar = backtest_progress_bar();
    progress_bar.set_position(0);
    progress_bar.set_message("initializing | value $100000.00 | 0 trading days");
    progress_bar.tick();
    let progress = {
        let progress_bar = progress_bar.clone();
        let progress_tx = progress_tx.clone();
        let last_drawn_percent = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let last_drawn_percent_for_progress = last_drawn_percent.clone();
        let last_catalog_date = Arc::new(std::sync::Mutex::new(None));
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
            let mut last_date = last_catalog_date
                .lock()
                .expect("backtest progress date poisoned");
            if last_date.as_ref() != Some(&progress.current_date) {
                *last_date = Some(progress.current_date);
                let _ = progress_tx.send(progress);
            }
        })
    };
    let data_feed_options = rlean_engine::data_feed::DataFeedOptions::default();
    let config = rlean_engine::BacktestRunConfig {
        data_sidecar,
        _compression_level: 3,
        start_date_override,
        end_date_override,
        parameters,
        data_feed_options,
        output_dir: None,
        artifact_sink: None,
        progress: Some(progress),
    };

    let runtime_context = rlean_engine::AlgorithmRuntimeContext::new(
        config.data_sidecar.clone(),
        config.parameters.clone(),
    );

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
    drop(progress_tx);
    if let Err(progress_error) = progress_task
        .await
        .context("join Verglas progress writer")?
    {
        let error = anyhow::anyhow!("persist backtest progress: {progress_error}");
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
    run_catalog.record_completed(&results).await?;
    println!("Results: verglas://rlean/runs/{}", run_catalog.run_id());

    Ok(())
}
