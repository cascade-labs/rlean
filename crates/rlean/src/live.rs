use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use rlean_algorithm::qc_algorithm::{AccountType, BrokerageModel, BrokerageName};

use crate::cli::LiveArgs;
use crate::config;
use crate::live_deployments::{
    launch_live_detached, run_live_control, update_live_deployment_status,
};
use crate::runtime::{
    connect_verglas, ensure_python_baseline_packages, execution_brokerage,
    historical_data_provider, live_data_provider, parse_algorithm_parameters_for_strategy,
    resolve_strategy_file, validate_strategy_path,
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
    let historical_provider = historical_data_provider(&args, verglas).await?;
    let live_data_provider = live_data_provider(&args)?;

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
    let brokerage_model = live_brokerage_model_for_name(requested_brokerage)?;

    let parameters = parse_algorithm_parameters_for_strategy(&args.strategy, &args.parameters)?;

    let ext = args
        .strategy
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext != "py" {
        bail!("Live trading currently supports Python strategies only");
    }

    // Build the artifact sink for this deployment. Live always keeps the local
    // deploy dir (the live control surface reads it); `s3`-only is treated like
    // `mirror` so we never delete the local dir out from under the control layer.
    let artifact_sink = build_live_artifact_sink(&global_config, deploy_dir.as_deref(), &args)?;

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
        output_dir: deploy_dir,
        artifact_sink,
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

    let result = match rlean_engine::runner::live::run_live_with_runtime(
        adapter,
        live_config,
        runtime_context,
    )
    .await
    {
        Ok(result) => result,
        Err(error) if rlean_sdk::interrupt::is_interrupted_error(&error) => {
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

/// Build the artifact sink for a live deployment.
///
/// Returns `None` when there is no deploy dir (nothing to mirror) or the mode
/// is local. For live, `s3`-only is treated like `mirror`: the local deploy dir
/// is always kept because the live control surface reads it, and snapshots are
/// mirrored to S3 asynchronously.
fn build_live_artifact_sink(
    global_config: &config::GlobalConfig,
    deploy_dir: Option<&std::path::Path>,
    args: &crate::cli::RunArgs,
) -> Result<Option<std::sync::Arc<rlean_engine::RunArtifactSink>>> {
    let Some(dir) = deploy_dir else {
        return Ok(None);
    };
    let artifact_config = crate::artifacts_config::resolve(
        args.artifact_store.as_deref(),
        args.artifact_s3.as_deref(),
        global_config,
    )?;
    if artifact_config.mode == rlean_engine::ArtifactStoreMode::Local {
        return Ok(None);
    }
    // Never use an s3-only temp buffer for live — force local-primary mirroring.
    let mode = rlean_engine::ArtifactStoreMode::Mirror;
    let project = live_project_name(dir, &args.strategy);
    let deploy_id = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&project)
        .to_string();
    let sink = rlean_engine::RunArtifactSink::new(
        mode,
        rlean_engine::RunKind::Live,
        dir.to_path_buf(),
        &project,
        &deploy_id,
        artifact_config.s3.as_ref(),
    );
    Ok(Some(std::sync::Arc::new(sink)))
}

/// Resolve the project name for a live deployment's S3 keys.
///
/// The strategy path inside a deployment is the code snapshot
/// (`<project>/live/<deploy-id>/code/main.py`), so deriving the name from the
/// file's parent dir would yield `code`. Prefer the recorded metadata:
///
/// 1. `deployment.json` in the deploy dir (`strategy_name` field),
/// 2. the project dir name when the deploy dir sits at `<project>/live/<id>`,
/// 3. the strategy file name as a last resort.
fn live_project_name(deploy_dir: &std::path::Path, strategy: &std::path::Path) -> String {
    if let Ok(text) = std::fs::read_to_string(deploy_dir.join("deployment.json")) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(name) = value.get("strategy_name").and_then(|v| v.as_str()) {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    if let Some(live_dir) = deploy_dir.parent() {
        if live_dir.file_name().and_then(|n| n.to_str()) == Some("live") {
            if let Some(project) = live_dir
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
            {
                return project.to_string();
            }
        }
    }
    crate::runtime::strategy_name_from_path(strategy)
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

    /// A deployment's strategy path is the code snapshot
    /// (`<project>/live/<deploy-id>/code/main.py`). The S3 project component
    /// must resolve to the project name, never the `code` snapshot dir.
    #[test]
    fn test_live_project_name_prefers_deployment_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let deploy_dir = tmp
            .path()
            .join("uw_control")
            .join("live")
            .join("2026-07-08_060145_uw_control");
        let code_dir = deploy_dir.join("code");
        std::fs::create_dir_all(&code_dir).unwrap();
        let strategy = code_dir.join("main.py");
        std::fs::write(&strategy, "pass").unwrap();
        std::fs::write(
            deploy_dir.join("deployment.json"),
            r#"{"deploy_id":"2026-07-08_060145_uw_control","strategy_name":"uw_control"}"#,
        )
        .unwrap();

        assert_eq!(live_project_name(&deploy_dir, &strategy), "uw_control");
    }

    #[test]
    fn test_live_project_name_falls_back_to_project_dir() {
        // No deployment.json: derive the project from <project>/live/<deploy-id>.
        let tmp = tempfile::tempdir().unwrap();
        let deploy_dir = tmp
            .path()
            .join("uw_control")
            .join("live")
            .join("2026-07-08_060145_uw_control");
        let code_dir = deploy_dir.join("code");
        std::fs::create_dir_all(&code_dir).unwrap();
        let strategy = code_dir.join("main.py");
        std::fs::write(&strategy, "pass").unwrap();

        assert_eq!(live_project_name(&deploy_dir, &strategy), "uw_control");
    }

    #[test]
    fn test_live_project_name_last_resort_uses_strategy_path() {
        // Exotic layout without live/<id> structure or metadata: fall back to
        // the strategy file's name derivation.
        let tmp = tempfile::tempdir().unwrap();
        let deploy_dir = tmp.path().join("somewhere");
        std::fs::create_dir_all(&deploy_dir).unwrap();
        let strategy_dir = tmp.path().join("my_algo");
        std::fs::create_dir_all(&strategy_dir).unwrap();
        let strategy = strategy_dir.join("main.py");
        std::fs::write(&strategy, "pass").unwrap();

        assert_eq!(live_project_name(&deploy_dir, &strategy), "my_algo");
    }
}
