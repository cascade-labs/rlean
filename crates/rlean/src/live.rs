use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use lean_algorithm::qc_algorithm::BrokerageName;

use crate::cli::LiveArgs;
use crate::live_deployments::{
    launch_live_detached, run_live_control, update_live_deployment_status,
};
use crate::runtime::{
    build_providers, ensure_python_baseline_packages, parse_algorithm_parameters_for_strategy,
    provider_args, resolve_configured_data_folder, resolve_datastore_for_data_root,
    resolve_strategy_file, validate_strategy_path,
};
use crate::{config, providers};

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
    let strategy_path = args.strategy.clone();
    resolve_configured_data_folder(&strategy_path, &mut args.data, &global_config)?;
    let datastore = resolve_datastore_for_data_root(&args.data, &global_config)?;
    args.data = datastore.data_root.clone();
    tracing::info!("Data folder: {}", args.data.display());

    let live_provider_names = args
        .data_provider_live
        .as_deref()
        .or(args.data_provider_historical.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live trading requires --data-provider-live, for example --data-provider-live tradier"
            )
        })?;
    let requested_brokerage = args
        .brokerage
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live mode requires --brokerage paper or a live brokerage plugin, for example --brokerage tradier"
            )
        })?;
    let requested_paper_brokerage = is_paper_brokerage_name(requested_brokerage);

    ensure_python_baseline_packages()?;
    use lean_python_runtime::AlgorithmImports;
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::Python::initialize();

    let provider_args = provider_args(args.data.clone(), Some(datastore.store.clone()), &args);
    let live_data_queue =
        providers::build_live_data_queue(live_provider_names, provider_args.clone())?;
    let brokerage = if requested_paper_brokerage {
        None
    } else {
        Some(providers::load_brokerage_plugin(
            requested_brokerage,
            &provider_args,
        )?)
    };
    let paper_trading = requested_paper_brokerage
        || brokerage
            .as_ref()
            .map(|brokerage| brokerage.uses_local_paper_fills())
            .unwrap_or(false);
    let brokerage_name = if requested_paper_brokerage {
        Some("Paper".to_string())
    } else {
        brokerage
            .as_ref()
            .map(|brokerage| brokerage.name().to_string())
    };
    let brokerage_model = live_brokerage_model_for_name(requested_brokerage);

    let history_provider = build_providers(&args, datastore.store.clone())?;
    let parameters = parse_algorithm_parameters_for_strategy(&args.strategy, &args.parameters)?;
    let custom_data_sources = providers::load_custom_data_plugins(&args.data);

    let ext = args
        .strategy
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext != "py" {
        bail!("Live trading currently supports Python strategies only");
    }

    let strategy_path = args.strategy.clone();
    let live_config = lean_engine::LiveRunConfig {
        data_root: datastore.data_root.clone(),
        data_store: datastore.store.clone(),
        history_provider,
        parameters,
        custom_data_sources,
        live_data_queue,
        brokerage,
        brokerage_model,
        paper_trading,
        max_slices: args.live_limits.live_max_slices,
        max_runtime: args
            .live_limits
            .live_max_runtime_seconds
            .map(Duration::from_secs),
        output_dir: deploy_dir,
    };
    let state = Arc::new(std::sync::Mutex::new(
        lean_algorithm::qc_algorithm::QcAlgorithm::new(
            "Algorithm",
            rust_decimal::Decimal::new(100000, 0),
        ),
    ));
    let runtime_context = lean_engine::AlgorithmRuntimeContext::new(
        live_config.data_root.clone(),
        live_config.data_store.clone(),
        live_config.history_provider.clone(),
        live_config.custom_data_sources.clone(),
        live_config.parameters.clone(),
    );
    let context = lean_sdk::algorithm::AlgorithmConstructionContext::new_with_runtime_services(
        state,
        Arc::new(runtime_context.clone()),
    );
    let adapter = lean_python_runtime::load_strategy_bridge_with_context(&strategy_path, context)?;

    let result = match lean_engine::runner::live::run_live_with_runtime(
        adapter,
        live_config,
        runtime_context,
    )
    .await
    {
        Ok(result) => result,
        Err(error) if lean_sdk::interrupt::is_interrupted_error(&error) => {
            std::process::exit(130);
        }
        Err(error) => return Err(error),
    };

    if let Some(brokerage_name) = brokerage_name {
        let mode = if paper_trading {
            "paper-fill"
        } else {
            "live-order"
        };
        println!(
            "Live brokerage {} run stopped: brokerage={} slices={} final_value=${:.2} order_events={} started_at={} stopped_at={}",
            mode,
            brokerage_name,
            result.slices_processed,
            result.final_value,
            result.order_events.len(),
            result.started_at.to_rfc3339(),
            result.stopped_at.to_rfc3339()
        );
    } else {
        println!(
            "Live paper run stopped: slices={} final_value=${:.2} order_events={} started_at={} stopped_at={}",
            result.slices_processed,
            result.final_value,
            result.order_events.len(),
            result.started_at.to_rfc3339(),
            result.stopped_at.to_rfc3339()
        );
    }
    Ok(())
}

fn live_brokerage_model_for_name(name: &str) -> Option<BrokerageName> {
    let normalized = normalized_brokerage_name(name);
    match normalized.as_str() {
        "tradier" | "tradierbrokerage" => Some(BrokerageName::TradierBrokerage),
        "hyperliquid" | "hyperliquidbrokerage" => Some(BrokerageName::HyperliquidBrokerage),
        "interactivebrokers" | "interactivebrokersbrokerage" | "ib" | "ibkr" => {
            Some(BrokerageName::InteractiveBrokersBrokerage)
        }
        "quantconnect" | "quantconnectbrokerage" => Some(BrokerageName::QuantConnectBrokerage),
        "default" | "paper" | "paperbrokerage" => Some(BrokerageName::Default),
        _ => None,
    }
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
            live_brokerage_model_for_name("tradier"),
            Some(BrokerageName::TradierBrokerage)
        );
        assert_eq!(
            live_brokerage_model_for_name("Tradier-Brokerage"),
            Some(BrokerageName::TradierBrokerage)
        );
        assert_eq!(
            live_brokerage_model_for_name("hyperliquid"),
            Some(BrokerageName::HyperliquidBrokerage)
        );
        assert_eq!(
            live_brokerage_model_for_name("PaperBrokerage"),
            Some(BrokerageName::Default)
        );
        assert!(is_paper_brokerage_name("paper"));
        assert!(is_paper_brokerage_name("PaperBrokerage"));
        assert!(!is_paper_brokerage_name("tradier"));
        assert_eq!(live_brokerage_model_for_name("custom"), None);
    }
}
