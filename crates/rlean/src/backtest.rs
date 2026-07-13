use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Result};
use lean_data_providers::IHistoryProvider;

use crate::cli::RunArgs;
use crate::runtime::{
    backtest_progress_bar, build_providers, ensure_python_baseline_packages,
    parse_algorithm_parameters_for_strategy, reserve_backtest_dir, resolve_configured_data_folder,
    resolve_datastore_for_data_root, strategy_name_from_path, update_backtests_latest_symlink,
    validate_strategy_path, ResolvedDataStore,
};
use crate::{config, providers};

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

    // Apply configured data-folder when --data was not explicitly provided.
    // Workspace rlean.json takes precedence over ~/.rlean/config, and relative
    // workspace paths are resolved from the directory containing rlean.json.
    // CLI --data-* flags win over env and config for the catalog connection.
    args.apply_data_catalog_overrides();
    let global_config = config::GlobalConfig::load()?;
    let strategy_path = args.strategy.clone();
    resolve_configured_data_folder(&strategy_path, &mut args.data, &global_config)?;
    let datastore = resolve_datastore_for_data_root(&args.data, &global_config).await?;
    args.data = datastore.data_root.clone();
    tracing::info!("Data folder: {}", args.data.display());

    let history_provider = build_providers(&args, datastore.store.clone())?;

    run_strategy_backtest(args, history_provider, datastore).await
}

async fn run_strategy_backtest(
    args: RunArgs,
    history_provider: Option<Arc<dyn IHistoryProvider>>,
    datastore: ResolvedDataStore,
) -> Result<()> {
    use lean_engine::report::{
        write_data_request_files, write_log_txt, write_order_events_json, write_orders_json,
        write_report, write_results_json, write_summary_json,
    };
    use lean_python_runtime::AlgorithmImports;

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
    let (backtest_dir, backtests_root) = if let Some(p) = args.report.clone() {
        std::fs::create_dir_all(&p)?;
        (p, None)
    } else {
        let backtests_root = args
            .strategy
            .parent()
            .map(|p| p.join("backtests"))
            .unwrap_or_else(|| PathBuf::from("backtests"));
        let now = chrono::Utc::now();
        let backtest_dir = reserve_backtest_dir(&backtests_root, now, &strategy_name)?;
        (backtest_dir, Some(backtests_root))
    };

    // Resolve the artifact store (local | s3 | mirror) and build the sink. The run
    // id is the reserved dir's final path component; the project is the strategy
    // name. The sink's working dir is where all run files (streaming + report)
    // are written: the real dir for local/mirror, a temp buffer for s3-only.
    let run_id = backtest_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&strategy_name)
        .to_string();
    let global_config = config::GlobalConfig::load()?;
    let artifact_config = crate::artifacts_config::resolve(
        args.artifact_store.as_deref(),
        args.artifact_s3.as_deref(),
        &global_config,
    )?;
    let sink = Arc::new(lean_engine::RunArtifactSink::new(
        artifact_config.mode,
        lean_engine::RunKind::Backtest,
        backtest_dir.clone(),
        &strategy_name,
        &run_id,
        artifact_config.s3.as_ref(),
    ));
    // All run files go into the sink's working dir. For local/mirror this is the
    // real backtest dir; for s3-only it is a temp buffer uploaded and deleted at
    // the end — remove the just-reserved (empty) local dir in that case.
    if !sink.writes_local_run_dir() {
        let _ = std::fs::remove_dir(&backtest_dir);
    }
    let backtest_dir = sink.working_dir().to_path_buf();

    let code_dir = backtest_dir.join("code");
    if let Err(e) = std::fs::create_dir_all(&code_dir) {
        eprintln!("Warning: could not create code snapshot dir: {e}");
    } else {
        let dest = code_dir.join(
            args.strategy
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("main.py")),
        );
        if let Err(e) = std::fs::copy(&args.strategy, &dest) {
            eprintln!("Warning: could not snapshot strategy code: {e}");
        }
    }

    let custom_data_sources = providers::load_custom_data_plugins(&args.data);
    let progress_bar = backtest_progress_bar();
    progress_bar.set_position(0);
    progress_bar.set_message("initializing | value $100000.00 | 0 trading days");
    progress_bar.tick();
    let progress = {
        let progress_bar = progress_bar.clone();
        let last_drawn_percent = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
        let last_drawn_percent_for_progress = last_drawn_percent.clone();
        Arc::new(move |progress: lean_engine::BacktestProgress| {
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
    let data_feed_options = lean_engine::data_feed::DataFeedOptions {
        fetch_missing_custom_data: args.data_provider_historical.is_some(),
        ..Default::default()
    };
    let config = lean_engine::BacktestRunConfig {
        data_root: datastore.data_root.clone(),
        data_store: datastore.store.clone(),
        _compression_level: 3,
        history_provider,
        start_date_override,
        end_date_override,
        parameters,
        custom_data_sources,
        data_feed_options,
        output_dir: Some(backtest_dir.clone()),
        artifact_sink: Some(sink.clone()),
        progress: Some(progress),
    };

    let runtime_context = lean_engine::AlgorithmRuntimeContext::new(
        config.data_root.clone(),
        config.data_store.clone(),
        config.history_provider.clone(),
        config.custom_data_sources.clone(),
        config.parameters.clone(),
    );

    let bridge: Box<dyn lean_sdk::AlgorithmBridge> = match ext {
        "py" => {
            let strategy_path = args.strategy.clone();
            let state = Arc::new(std::sync::Mutex::new(
                lean_algorithm::qc_algorithm::QcAlgorithm::new(
                    "Algorithm",
                    rust_decimal::Decimal::new(100000, 0),
                ),
            ));
            let context =
                lean_sdk::algorithm::AlgorithmConstructionContext::new_with_runtime_services(
                    state,
                    Arc::new(runtime_context.clone()),
                );
            let adapter =
                lean_python_runtime::load_strategy_bridge_with_context(&strategy_path, context)?;
            Box::new(adapter)
        }
        "so" | "dylib" => load_native_strategy_bridge(&args.strategy)?,
        other => bail!(
            "Unknown strategy extension '.{}'. Expected .py, .so, or .dylib",
            other
        ),
    };

    let results = match lean_engine::runner::backtest::run_backtest_with_runtime(
        bridge,
        config,
        runtime_context,
    )
    .await
    {
        Ok(results) => results,
        Err(error) if lean_sdk::interrupt::is_interrupted_error(&error) => {
            std::process::exit(130);
        }
        Err(error) => return Err(error),
    };

    progress_bar.finish_with_message(format!(
        "{} | value ${:.2} | {} trading days",
        results.end_date, results.final_value, results.trading_days
    ));
    results.print_summary();

    // The backtest ID (Unix epoch seconds at backtest start) is used as the
    // filename prefix for all per-backtest files, matching C# LEAN's convention.
    let id = results.backtest_id;
    // Millisecond timestamp suffix for data-request files.
    let ts_ms = chrono::Utc::now().format("%Y%m%d%H%M%S%3f");

    // ── write all output files ────────────────────────────────────────────────
    let json_path = backtest_dir.join(format!("{id}.json"));
    let order_events_path = backtest_dir.join(format!("{id}-order-events.json"));
    let orders_path = backtest_dir.join(format!("{id}-orders.json"));
    let summary_path = backtest_dir.join(format!("{id}-summary.json"));
    let id_log_path = backtest_dir.join(format!("{id}-log.txt"));
    let top_log_path = backtest_dir.join("log.txt");
    let succeeded_path = backtest_dir.join(format!("succeeded-data-requests-{ts_ms}.txt"));
    let failed_path = backtest_dir.join(format!("failed-data-requests-{ts_ms}.txt"));
    let report_path = backtest_dir.join("report.html");

    if let Err(e) = write_results_json(&results, &json_path) {
        eprintln!("Failed to write results: {e}");
    }
    if let Err(e) = write_order_events_json(&results, &order_events_path) {
        eprintln!("Failed to write order events: {e}");
    }
    if let Err(e) = write_orders_json(&results, &orders_path) {
        eprintln!("Failed to write orders: {e}");
    }
    if let Err(e) = write_summary_json(&results, &summary_path) {
        eprintln!("Failed to write summary: {e}");
    }
    if let Err(e) = write_log_txt(&results, &id_log_path) {
        eprintln!("Failed to write log: {e}");
    }
    let _ = std::fs::copy(&id_log_path, &top_log_path);
    if let Err(e) = write_data_request_files(&results, &succeeded_path, &failed_path) {
        eprintln!("Failed to write data requests: {e}");
    }
    if let Err(e) = write_report(&results, &report_path) {
        eprintln!("Failed to write report: {e}");
    }

    // Final artifact upload: mirror the complete run dir (streaming + report
    // files) to S3, bypassing the per-file checkpoint throttle. In s3-only mode
    // this uploads then deletes the temp working buffer when `sink` is dropped.
    sink.flush_all();

    // Only maintain the local `latest` symlink when we actually kept a local run
    // dir (skip in s3-only mode, whose working dir is a temp buffer).
    if sink.writes_local_run_dir() {
        if let Some(root) = backtests_root {
            if let Err(e) = update_backtests_latest_symlink(&root, &backtest_dir) {
                eprintln!("Warning: could not update backtests/latest symlink: {e}");
            }
        }
    }

    match sink.mode() {
        lean_engine::ArtifactStoreMode::S3 => {
            if let Some(base) = sink.s3_key_base() {
                println!(
                    "Results: s3://{}/{}",
                    s3_bucket_display(&artifact_config),
                    base
                );
            }
        }
        _ => {
            println!("Results: {}", backtest_dir.display());
            if let Some(base) = sink.s3_key_base() {
                println!(
                    "Mirrored to: s3://{}/{}",
                    s3_bucket_display(&artifact_config),
                    base
                );
            }
        }
    }

    Ok(())
}

/// Bucket name for display in the "Results" / "Mirrored to" lines.
fn s3_bucket_display(config: &crate::artifacts_config::ArtifactConfig) -> String {
    config
        .s3
        .as_ref()
        .map(|s| s.bucket.clone())
        .unwrap_or_default()
}

fn load_native_strategy_bridge(strategy_path: &Path) -> Result<Box<dyn lean_sdk::AlgorithmBridge>> {
    use lean_algorithm::algorithm::QcAlgorithmStrategy;
    use libloading::{Library, Symbol};

    // Safety: the plugin must export `create_algorithm` with C ABI.
    let lib = unsafe { Library::new(strategy_path) }
        .map_err(|e| anyhow::anyhow!("Failed to load plugin '{}': {e}", strategy_path.display()))?;

    let lib = Box::leak(Box::new(lib));
    let create: Symbol<unsafe extern "C" fn() -> Box<dyn QcAlgorithmStrategy>> =
        unsafe { lib.get(b"create_algorithm\0") }
            .map_err(|_| anyhow::anyhow!(
                "Plugin does not export `create_algorithm`. \
                 Add `#[no_mangle] pub extern \"C\" fn create_algorithm() -> Box<dyn QcAlgorithmStrategy>` to your strategy crate."
            ))?;

    let strategy = unsafe { create() };
    Ok(Box::new(lean_sdk::QcAlgorithmNativeBridge::new(strategy)))
}
