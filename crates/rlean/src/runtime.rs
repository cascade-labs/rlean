use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use rlean_brokerages::{
    Brokerage, HttpBrokerage, HttpBrokerageConfig, TradierBrokerage, TradierBrokerageConfig,
    TradierEnvironment as TradierBrokerageEnvironment,
};
use rlean_data_providers::LiveDataProvider;
use rlean_data_providers::{
    CacheFirstHistoryProvider, HistoricalDataProvider, MassiveConfig,
    MassiveHistoricalDataProvider, MassiveLiveConfig, MassiveLiveDataProvider,
    RoutedLiveDataProvider, ThetaDataConfig, ThetaDataHistoricalDataProvider,
    TradierEnvironment as TradierDataEnvironment, TradierLiveDataProvider, TradierMarketDataConfig,
    VerglasCustomLiveDataProvider, VerglasHistoricalDataStore,
};
use verglas_sdk::{Client as VerglasClient, ConnectOptions};

use crate::cli::RunArgs;
use crate::config;

pub(crate) fn backtest_progress_bar() -> indicatif::ProgressBar {
    let bar = indicatif::ProgressBar::new(100);
    let style = indicatif::ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos:>3}% {msg}",
    )
    .unwrap_or_else(|_| indicatif::ProgressStyle::default_bar())
    .progress_chars("=> ");
    bar.set_style(style);
    bar.enable_steady_tick(Duration::from_millis(250));
    bar
}

pub(crate) async fn historical_data_provider(
    args: &RunArgs,
    verglas: VerglasClient,
) -> Result<Arc<dyn HistoricalDataProvider>> {
    let name = args
        .data_provider_historical
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("a historical data provider is required"))?;
    let provider: Arc<dyn HistoricalDataProvider> = match name {
        "massive" => {
            let integration = config::IntegrationConfigs::load()?.get_integration("massive");
            let api_key = integration
                .get("api_key")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "historical provider massive requires integration config key massive.api_key"
                    )
                })?;
            Arc::new(MassiveHistoricalDataProvider::new(MassiveConfig::new(
                api_key,
            ))?)
        }
        "thetadata" => {
            let integration = config::IntegrationConfigs::load()?.get_integration("thetadata");
            let api_key = optional_string(&integration, "api_key");
            let mut provider_config = ThetaDataConfig::new(api_key);
            if let Some(base_url) = optional_string(&integration, "base_url") {
                provider_config.base_url = base_url;
            }
            if let Some(max_concurrent) = optional_usize(&integration, "max_concurrent") {
                provider_config.max_concurrent = max_concurrent.max(1);
            }
            if let Some(requests_per_second) = optional_f64(&integration, "requests_per_second") {
                provider_config.requests_per_second = requests_per_second;
            }
            Arc::new(ThetaDataHistoricalDataProvider::new(provider_config)?)
        }
        other => bail!("unsupported historical data provider '{other}'"),
    };
    let store = Arc::new(VerglasHistoricalDataStore::new(verglas).await?);
    let provider = CacheFirstHistoryProvider::new(store, vec![provider])?;
    tracing::info!(
        provider = name,
        "Configured cache-first historical data provider"
    );
    Ok(Arc::new(provider))
}

pub(crate) async fn live_data_provider(
    args: &RunArgs,
    verglas: VerglasClient,
) -> Result<Arc<dyn LiveDataProvider>> {
    let name = args
        .live_data_feed
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("live trading requires --live-data-feed"))?;
    let market: Arc<dyn LiveDataProvider> = match name {
        "massive" => {
            let integration = config::IntegrationConfigs::load()?.get_integration("massive");
            let api_key = required_string(&integration, "api_key", "massive")?;
            Arc::new(MassiveLiveDataProvider::new(MassiveLiveConfig::new(
                api_key,
            ))?)
        }
        "tradier" => {
            let integration = config::IntegrationConfigs::load()?.get_integration("tradier");
            let token = required_string(&integration, "access_token", "tradier")?;
            let environment = match optional_string(&integration, "environment").as_deref() {
                Some("paper") | Some("sandbox") => TradierDataEnvironment::Paper,
                _ => TradierDataEnvironment::Live,
            };
            Arc::new(TradierLiveDataProvider::new(TradierMarketDataConfig::new(
                token,
                environment,
            ))?)
        }
        other => bail!("unsupported live data provider '{other}'"),
    };
    let custom: Arc<dyn LiveDataProvider> =
        Arc::new(VerglasCustomLiveDataProvider::new(verglas).await?);
    Ok(Arc::new(RoutedLiveDataProvider::new(market, custom)))
}

pub(crate) fn execution_brokerage(args: &RunArgs) -> Result<Option<Box<dyn Brokerage>>> {
    let name = args
        .brokerage
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("live trading requires --brokerage"))?;
    if matches!(name, "paper" | "paperbrokerage") {
        return Ok(None);
    }
    match name {
        "http" => {
            let url = args
                .brokerage_url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("HTTP brokerage requires --brokerage-url"))?;
            Ok(Some(Box::new(HttpBrokerage::new(
                HttpBrokerageConfig::new(url, args.brokerage_account.clone()),
            )?)))
        }
        "tradier" | "tradierbrokerage" => {
            let integration = config::IntegrationConfigs::load()?.get_integration("tradier");
            let token = required_string(&integration, "access_token", "tradier")?;
            let account = args
                .brokerage_account
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .or_else(|| optional_string(&integration, "account_number"))
                .ok_or_else(|| anyhow::anyhow!("Tradier brokerage requires --brokerage-account"))?;
            let environment = match optional_string(&integration, "environment").as_deref() {
                Some("paper") | Some("sandbox") => TradierBrokerageEnvironment::Paper,
                _ => TradierBrokerageEnvironment::Live,
            };
            Ok(Some(Box::new(TradierBrokerage::new(
                TradierBrokerageConfig::new(token, account, environment),
            )?)))
        }
        other => bail!("unsupported execution brokerage '{other}'"),
    }
}

fn required_string(
    values: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    integration: &str,
) -> Result<String> {
    optional_string(values, key).ok_or_else(|| {
        anyhow::anyhow!("{integration} requires integration config key {integration}.{key}")
    })
}

fn optional_string(
    values: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    values
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn optional_usize(values: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<usize> {
    values.get(key).and_then(|value| {
        value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .or_else(|| value.as_str()?.trim().parse().ok())
    })
}

fn optional_f64(values: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<f64> {
    values.get(key).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str()?.trim().parse().ok())
    })
}

/// Connects once to the configured Verglas gateway. The SDK discovers the
/// catalog, query, and write services and applies one bearer token to all of
/// them. Environment values override the persisted machine configuration.
pub(crate) async fn connect_verglas(global_config: &config::GlobalConfig) -> Result<VerglasClient> {
    let endpoint = std::env::var("VERGLAS_ENDPOINT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| global_config.verglas_endpoint.clone())
        .unwrap_or_else(|| "http://127.0.0.1:8334".to_owned());
    let token = std::env::var("VERGLAS_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| global_config.verglas_token.clone());
    let mut options = ConnectOptions::new(endpoint.clone());
    if let Some(token) = token {
        options = options.with_token(token);
    }
    VerglasClient::connect(options)
        .await
        .with_context(|| format!("connect to Verglas gateway at {endpoint}"))
}

pub(crate) fn resolve_strategy_file(path: PathBuf) -> Result<PathBuf> {
    if path.is_dir() {
        let candidate = path.join("main.py");
        if candidate.exists() {
            return Ok(candidate);
        }
        bail!(
            "'{}' is a directory but contains no main.py. \
             Pass the strategy file directly or run `rlean create-project` to scaffold one.",
            path.display()
        );
    }
    Ok(path)
}

pub(crate) fn parse_algorithm_parameters_for_strategy(
    strategy: &Path,
    raw: &[String],
) -> Result<HashMap<String, String>> {
    let mut parameters = project_config_parameters(strategy)?;
    parameters.extend(parse_algorithm_parameters(raw)?);
    Ok(parameters)
}

fn parse_algorithm_parameters(raw: &[String]) -> Result<HashMap<String, String>> {
    let mut parameters = HashMap::new();
    for item in raw {
        let Some((key, value)) = item.split_once('=') else {
            bail!("invalid algorithm parameter '{item}', expected KEY=VALUE");
        };
        if key.is_empty() {
            bail!("invalid algorithm parameter '{item}', key cannot be empty");
        }
        parameters.insert(key.to_string(), value.to_string());
    }
    Ok(parameters)
}

fn project_config_parameters(strategy: &Path) -> Result<HashMap<String, String>> {
    let project_dir = strategy.parent().unwrap_or_else(|| Path::new("."));
    let path = project_dir.join("config.json");
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let config: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("Failed to parse {}", path.display()))?;
    let Some(parameters) = config.get("parameters") else {
        return Ok(HashMap::new());
    };
    let Some(parameters) = parameters.as_object() else {
        bail!("project config parameters must be an object");
    };

    parameters
        .clone()
        .into_iter()
        .map(|(key, value)| Ok((key, project_parameter_value_to_string(value)?)))
        .collect()
}

fn project_parameter_value_to_string(value: serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Null => bail!("project config parameter values cannot be null"),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            bail!("project config parameter values must be strings, numbers, or booleans")
        }
    }
}

pub(crate) fn ensure_python_baseline_packages() -> Result<()> {
    let python = std::env::var("RLEAN_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let packages = ["numpy", "pandas", "scipy"];
    let site_packages = rlean_python_site_packages(&python)?;
    let import_check = packages
        .iter()
        .map(|pkg| format!("import {pkg}"))
        .collect::<Vec<_>>()
        .join("; ");

    let check = python_import_check(&python, &site_packages, &import_check);
    if matches!(check, Ok(status) if status.success()) {
        return Ok(());
    }

    eprintln!(
        "Python baseline packages missing for {python}; installing LEAN-compatible defaults into {}: {}",
        site_packages.display(),
        packages.join(", ")
    );

    std::fs::create_dir_all(&site_packages)?;
    if install_python_baseline_with_uv(&site_packages, packages).is_err() {
        install_python_baseline_with_pip(&python, packages)?;
    }

    let recheck = python_import_check(&python, &site_packages, &import_check)
        .map_err(|e| anyhow::anyhow!("failed to verify Python baseline packages: {e}"))?;
    if !recheck.success() {
        bail!(
            "Python baseline packages still cannot be imported by {python}. \
             Set RLEAN_PYTHON to the interpreter used by the embedded Python runtime."
        );
    }

    Ok(())
}

fn python_import_check(
    python: &str,
    site_packages: &Path,
    import_check: &str,
) -> std::io::Result<std::process::ExitStatus> {
    ProcessCommand::new(python)
        .env("PYTHONPATH", site_packages)
        .arg("-c")
        .arg(import_check)
        .status()
}

fn install_python_baseline_with_uv(site_packages: &Path, packages: [&str; 3]) -> Result<()> {
    let python_platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-manylinux_2_28",
        ("linux", "x86_64") => "x86_64-manylinux_2_28",
        _ => "aarch64-apple-darwin",
    };

    let status = ProcessCommand::new("uv")
        .args([
            "pip",
            "install",
            "--target",
            site_packages.to_string_lossy().as_ref(),
            "--python-version",
            "3.14",
            "--python-platform",
            python_platform,
        ])
        .args(packages)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `uv pip install`: {e}"))?;
    if !status.success() {
        bail!("`uv pip install` failed");
    }
    Ok(())
}

fn install_python_baseline_with_pip(python: &str, packages: [&str; 3]) -> Result<()> {
    let ensurepip_status = ProcessCommand::new(python)
        .args(["-m", "ensurepip", "--upgrade"])
        .status();
    if !matches!(ensurepip_status, Ok(status) if status.success()) {
        eprintln!("Warning: `{python} -m ensurepip --upgrade` did not complete successfully");
    }

    let status = ProcessCommand::new(python)
        .args(["-m", "pip", "install", "--upgrade"])
        .args(packages)
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to run `{python} -m pip install --upgrade numpy pandas scipy`: {e}"
            )
        })?;
    if !status.success() {
        bail!(
            "failed to install Python baseline packages for {python}. \
             Install them manually with `{python} -m pip install --upgrade numpy pandas scipy`, \
             or set RLEAN_PYTHON to the Python interpreter rlean should use."
        );
    }
    Ok(())
}

fn rlean_python_site_packages(python: &str) -> Result<PathBuf> {
    let tag = ProcessCommand::new(python)
        .arg("-c")
        .arg("import sys; print(f'cp{sys.version_info.major}{sys.version_info.minor}')")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to query Python version from {python}: {e}"))?;
    if !tag.status.success() {
        bail!("failed to query Python version from {python}");
    }
    let tag = String::from_utf8(tag.stdout)
        .context("Python version output was not UTF-8")?
        .trim()
        .to_string();

    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".rlean")
        .join("python")
        .join(tag)
        .join("site-packages"))
}

/// Derive a human-readable strategy name from a strategy file path.
///
/// Rules (matching C# LEAN's project-name convention):
///  - If the file is `main.py`, use the parent directory name.
///  - Otherwise use the file stem (filename without extension).
///  - Falls back to `"strategy"` when neither can be determined.
pub(crate) fn strategy_name_from_path(strategy: &Path) -> String {
    let stem = strategy
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("strategy")
        .to_string();
    if stem == "main" {
        strategy
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("strategy")
            .to_string()
    } else {
        stem
    }
}

/// Stable timestamped name used for live deployment snapshots.
pub(crate) fn backtest_dir_name(
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> String {
    format!("{}_{}", datetime.format("%Y-%m-%d_%H%M%S"), strategy_name)
}

pub(crate) fn validate_strategy_path(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("Strategy file not found: {}", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_strategy_name_main_py_uses_parent_dir() {
        let p = Path::new("sma_crossover/main.py");
        assert_eq!(strategy_name_from_path(p), "sma_crossover");
    }

    #[test]
    fn test_strategy_name_non_main_uses_stem() {
        let p = Path::new("sma_crossover/my_algo.py");
        assert_eq!(strategy_name_from_path(p), "my_algo");
    }

    #[test]
    fn test_strategy_name_absolute_path_main_py() {
        let p = Path::new("/home/user/strategies/etf_blend/main.py");
        assert_eq!(strategy_name_from_path(p), "etf_blend");
    }

    #[test]
    fn test_strategy_name_absolute_path_named_file() {
        let p = Path::new("/home/user/strategies/signal_generator.py");
        assert_eq!(strategy_name_from_path(p), "signal_generator");
    }

    #[test]
    fn test_strategy_name_non_main_file() {
        let p = Path::new("strategies/my_strategy.py");
        assert_eq!(strategy_name_from_path(p), "my_strategy");
    }

    #[test]
    fn test_project_config_parameters_are_loaded_and_cli_overrides() {
        let root =
            std::env::temp_dir().join(format!("rlean-project-params-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.py"), "").unwrap();
        std::fs::write(
            root.join("config.json"),
            r#"{
  "algorithm-language": "python",
  "parameters": {
    "max_holds": "30",
    "enabled": true,
    "threshold": 2.5
  },
  "description": "",
  "local-id": 123
}"#,
        )
        .unwrap();

        let parameters = parse_algorithm_parameters_for_strategy(
            &root.join("main.py"),
            &["max_holds=12".to_string(), "extra=value".to_string()],
        )
        .unwrap();

        assert_eq!(parameters.get("max_holds").map(String::as_str), Some("12"));
        assert_eq!(parameters.get("enabled").map(String::as_str), Some("true"));
        assert_eq!(parameters.get("threshold").map(String::as_str), Some("2.5"));
        assert_eq!(parameters.get("extra").map(String::as_str), Some("value"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_project_config_parameters_do_not_require_project_metadata() {
        let root = std::env::temp_dir().join(format!(
            "rlean-project-params-minimal-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.py"), "").unwrap();
        std::fs::write(
            root.join("config.json"),
            r#"{"environment": "backtesting"}"#,
        )
        .unwrap();

        let parameters = parse_algorithm_parameters_for_strategy(
            &root.join("main.py"),
            &["max_holds=12".to_string()],
        )
        .unwrap();

        assert_eq!(parameters.get("max_holds").map(String::as_str), Some("12"));
        assert_eq!(parameters.len(), 1);

        let _ = std::fs::remove_dir_all(&root);
    }
}
