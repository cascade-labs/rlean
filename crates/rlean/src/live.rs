use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rlean_algorithm::qc_algorithm::{AccountType, BrokerageModel, BrokerageName};

use crate::cli::LiveArgs;
use crate::config;
use crate::live_deployments::{
    launch_live_detached, run_live_control, update_live_deployment_status,
};
use crate::runtime::{
    connect_verglas, ensure_python_baseline_packages, execution_brokerage,
    historical_data_provider, live_data_provider, parse_algorithm_parameters_for_strategy,
    resolve_strategy_file, strategy_name_from_path, validate_strategy_path,
};

pub(crate) async fn run(args: LiveArgs) -> Result<()> {
    if let Some(command) = args.command.clone() {
        return run_live_control(command);
    }

    if args.foreground {
        if let Some(dir) = args.live_deploy_dir.as_ref() {
            update_live_deployment_status(dir, "running", None, None, Some(std::process::id()));
        }
        let deploy_dir = args.live_deploy_dir.clone();
        let result = run_live_foreground(args).await;
        if let Some(dir) = deploy_dir.as_ref() {
            match &result {
                Ok(()) => update_live_deployment_status(dir, "stopped", None, Some(0), None),
                Err(error) => update_live_deployment_status(
                    dir,
                    "runtime-error",
                    Some(error.to_string()),
                    Some(1),
                    None,
                ),
            }
        }
        return result;
    }

    launch_live_detached(args)
}

async fn run_live_foreground(args: LiveArgs) -> Result<()> {
    let deploy_dir = args.live_deploy_dir.clone();
    let mut args = args.to_run_args()?;
    args.strategy = resolve_strategy_file(args.strategy)?;
    if let Ok(canonical) = std::fs::canonicalize(&args.strategy) {
        args.strategy = canonical;
    }
    if args.strategy.is_dir() {
        args.strategy = resolve_strategy_file(args.strategy)?;
    }

    validate_strategy_path(&args.strategy)?;

    let global_config = config::GlobalConfig::load()?;
    let verglas = connect_verglas(&global_config).await?;
    let (historical_provider, dropped_cache_writes) =
        historical_data_provider(&args, verglas.clone()).await?;
    let live_data_provider = live_data_provider(&args, verglas.clone()).await?;

    let requested_brokerage = args
        .brokerage
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("live mode requires --brokerage"))?;
    let requested_paper_brokerage = is_paper_brokerage_name(requested_brokerage);
    let brokerage_account = args
        .brokerage_account
        .as_deref()
        .map(str::trim)
        .filter(|account| !account.is_empty());
    if requested_paper_brokerage && brokerage_account.is_some() {
        bail!("--brokerage-account cannot be used with the internal paper brokerage");
    }

    ensure_python_baseline_packages()?;
    use rlean_python_runtime::AlgorithmImports;
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::Python::initialize();

    let brokerage = execution_brokerage(&args)?;
    let paper_trading = requested_paper_brokerage;
    let brokerage_name = if requested_paper_brokerage {
        Some("Paper".to_string())
    } else {
        Some(requested_brokerage.to_string())
    };
    let brokerage_model_name = brokerage
        .as_deref()
        .and_then(rlean_brokerages::Brokerage::brokerage_model)
        .unwrap_or(requested_brokerage);
    let brokerage_model = live_brokerage_model_for_name(brokerage_model_name)?;

    let parameters = parse_algorithm_parameters_for_strategy(&args.strategy, &args.parameters)?;

    let ext = args
        .strategy
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext != "py" {
        bail!("Live trading currently supports Python strategies only");
    }

    let strategy_name = strategy_name_from_path(&args.strategy);
    let deploy_id = deploy_dir
        .as_ref()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .map(str::to_owned);
    let run_id = deploy_id
        .as_deref()
        .map(|id| format!("live-{id}"))
        .unwrap_or_else(|| format!("live-{}", uuid::Uuid::new_v4()));
    let deploy_started_at = chrono::Utc::now();

    let run_catalog = crate::run_catalog::RunCatalog::connect(
        verglas.clone(),
        run_id.clone(),
        strategy_name,
        &parameters,
    )
    .await?;
    let restore = crate::run_catalog::RunCatalog::load_live_restore(&verglas, &run_id)
        .await
        .context("load live restore state from Verglas")?;
    run_catalog.record_started().await?;

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

    let strategy_path = args.strategy.clone();
    let live_config = rlean_engine::LiveRunConfig {
        historical_provider: historical_provider.clone(),
        live_data_provider,
        parameters,
        brokerage,
        brokerage_model,
        paper_trading,
        max_slices: args.live_limits.live_max_slices,
        max_runtime: args
            .live_limits
            .live_max_runtime_seconds
            .map(Duration::from_secs),
        stream_updates: Some(stream_tx),
        deploy_started_at,
        restore,
        deploy_id,
    };
    let state = Arc::new(std::sync::Mutex::new(
        rlean_algorithm::qc_algorithm::QcAlgorithm::new(
            "Algorithm",
            rust_decimal::Decimal::new(100000, 0),
        ),
    ));
    let runtime_context = rlean_engine::AlgorithmRuntimeContext::new(
        historical_provider,
        live_config.parameters.clone(),
    );
    let context = rlean_sdk::algorithm::AlgorithmConstructionContext::new_with_runtime_services(
        state,
        Arc::new(runtime_context.clone()),
    );
    let adapter = rlean_python_runtime::load_strategy_bridge_with_context(&strategy_path, context)?;

    let run_result = rlean_engine::runner::live::run_live_with_runtime(
        adapter,
        live_config,
        runtime_context,
    )
    .await;

    // Drop the sender by ending the engine; then drain the catalog writer.
    let join_result = stream_task.await;
    run_catalog.log_dropped_appends_summary();
    dropped_cache_writes.log_summary();

    let result = match run_result {
        Ok(result) => result,
        Err(error) if rlean_sdk::interrupt::is_interrupted_error(&error) => {
            std::process::exit(130);
        }
        Err(error) => {
            let _ = run_catalog.record_failed(&error).await;
            return Err(error);
        }
    };
    join_result.context("live catalog stream task")??;

    // Terminal live row: reuse the completed path shape via a synthetic
    // BacktestRunResult is overkill; mark stopped with a final running→failed
    // style append that records final value on the runs table.
    run_catalog
        .record_live_stopped(result.final_value, result.slices_processed, result.order_events.len())
        .await?;

    if let Some(brokerage_name) = brokerage_name {
        let mode = if paper_trading {
            "paper-fill"
        } else {
            "live-order"
        };
        println!(
            "Live brokerage {} run stopped: brokerage={} slices={} final_value=${:.2} order_events={} started_at={} stopped_at={} run_id={}",
            mode,
            brokerage_name,
            result.slices_processed,
            result.final_value,
            result.order_events.len(),
            result.started_at.to_rfc3339(),
            result.stopped_at.to_rfc3339(),
            run_catalog.run_id(),
        );
    } else {
        println!(
            "Live paper run stopped: slices={} final_value=${:.2} order_events={} started_at={} stopped_at={} run_id={}",
            result.slices_processed,
            result.final_value,
            result.order_events.len(),
            result.started_at.to_rfc3339(),
            result.stopped_at.to_rfc3339(),
            run_catalog.run_id(),
        );
    }
    Ok(())
}

fn live_brokerage_model_for_name(name: &str) -> Result<BrokerageModel> {
    let normalized = normalized_brokerage_name(name);
    let model = match normalized.as_str() {
        "robinhood" | "robinhoodbrokerage" => {
            BrokerageModel::new(BrokerageName::RobinhoodBrokerage, AccountType::Cash)
        }
        "fidelity" | "fidelitybrokerage" => {
            BrokerageModel::new(BrokerageName::FidelityBrokerage, AccountType::Cash)
        }
        "tradier" | "tradierbrokerage" => {
            BrokerageModel::new(BrokerageName::TradierBrokerage, AccountType::Margin)
        }
        "hyperliquid" | "hyperliquidbrokerage" => {
            BrokerageModel::new(BrokerageName::HyperliquidBrokerage, AccountType::Margin)
        }
        "interactivebrokers" | "interactivebrokersbrokerage" | "ib" | "ibkr" => {
            BrokerageModel::new(
                BrokerageName::InteractiveBrokersBrokerage,
                AccountType::Margin,
            )
        }
        "quantconnect" | "quantconnectbrokerage" => {
            BrokerageModel::new(BrokerageName::QuantConnectBrokerage, AccountType::Margin)
        }
        "default" | "paper" | "paperbrokerage" => {
            BrokerageModel::new(BrokerageName::Default, AccountType::Margin)
        }
        _ => bail!(
            "execution brokerage '{name}' has no brokerage model; define its account type and order rules before using it live"
        ),
    };
    Ok(model)
}

pub(crate) fn is_paper_brokerage_name(name: &str) -> bool {
    matches!(
        normalized_brokerage_name(name).as_str(),
        "paper" | "paperbrokerage"
    )
}

fn normalized_brokerage_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_live_brokerage_model_name_mapping() {
        assert_eq!(
            live_brokerage_model_for_name("tradier").unwrap(),
            BrokerageModel::new(BrokerageName::TradierBrokerage, AccountType::Margin)
        );
        assert_eq!(
            live_brokerage_model_for_name("Tradier-Brokerage").unwrap(),
            BrokerageModel::new(BrokerageName::TradierBrokerage, AccountType::Margin)
        );
        assert_eq!(
            live_brokerage_model_for_name("hyperliquid").unwrap(),
            BrokerageModel::new(BrokerageName::HyperliquidBrokerage, AccountType::Margin)
        );
        assert_eq!(
            live_brokerage_model_for_name("PaperBrokerage").unwrap(),
            BrokerageModel::new(BrokerageName::Default, AccountType::Margin)
        );
        assert_eq!(
            live_brokerage_model_for_name("robinhood").unwrap(),
            BrokerageModel::new(BrokerageName::RobinhoodBrokerage, AccountType::Cash)
        );
        assert_eq!(
            live_brokerage_model_for_name("fidelity").unwrap(),
            BrokerageModel::new(BrokerageName::FidelityBrokerage, AccountType::Cash)
        );
        assert!(is_paper_brokerage_name("paper"));
        assert!(is_paper_brokerage_name("PaperBrokerage"));
        assert!(!is_paper_brokerage_name("tradier"));
        assert!(live_brokerage_model_for_name("custom").is_err());
    }
}
