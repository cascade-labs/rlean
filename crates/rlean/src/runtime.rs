use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use lean_data_providers::IHistoryProvider;
use lean_storage::IcebergStore;

use crate::cli::RunArgs;
use crate::{config, providers};

#[derive(Clone)]
pub(crate) struct ResolvedDataStore {
    pub(crate) store: Arc<IcebergStore>,
    pub(crate) data_root: PathBuf,
}

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

pub(crate) fn resolve_datastore_for_data_root(
    data_folder: &Path,
    global_config: &config::GlobalConfig,
) -> Result<ResolvedDataStore> {
    match global_config.datastore.as_str() {
        "file" => {
            let store = block_connect_iceberg_store(data_folder.to_path_buf())?;
            Ok(ResolvedDataStore {
                store,
                data_root: data_folder.to_path_buf(),
            })
        }
        "s3" => bail!("datastore=s3 is not supported until Iceberg S3 FileIO is wired"),
        other => bail!("unsupported datastore '{other}', expected 'file' or 's3'"),
    }
}

fn block_connect_iceberg_store(data_root: PathBuf) -> Result<Arc<IcebergStore>> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build Iceberg store runtime")?
            .block_on(IcebergStore::connect_local(data_root))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Iceberg store worker panicked"))?
    .map(Arc::new)
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

pub(crate) fn resolve_configured_data_folder(
    strategy: &Path,
    data: &mut PathBuf,
    global_config: &config::GlobalConfig,
) -> Result<()> {
    if data == Path::new("data") {
        if let Some(folder) =
            config::configured_data_folder_for_datastore(strategy, &global_config.datastore)?
        {
            *data = folder;
        }
    }
    Ok(())
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

/// Build the backtest output directory path in LEAN format:
///   `<backtests_root>/YYYY-MM-DD_HHMMSS_<strategy_name>`
pub(crate) fn backtest_dir_name(
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> String {
    format!("{}_{}", datetime.format("%Y-%m-%d_%H%M%S"), strategy_name)
}

pub(crate) fn reserve_backtest_dir(
    backtests_root: &Path,
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(backtests_root)?;
    let base = backtest_dir_name(datetime, strategy_name);
    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}_{}", attempt + 1)
        };
        let candidate = backtests_root.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!(
        "could not reserve unique backtest directory under {} for {}",
        backtests_root.display(),
        base
    )
}

/// Point `<backtests_root>/latest` at the most recently completed backtest directory.
pub(crate) fn update_backtests_latest_symlink(
    backtests_root: &Path,
    backtest_dir: &Path,
) -> Result<()> {
    let dir_name = backtest_dir
        .file_name()
        .context("backtest directory has no name")?;
    let latest = backtests_root.join("latest");

    match std::fs::symlink_metadata(&latest) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::remove_file(&latest)?;
        }
        Ok(_) => {
            bail!(
                "{} exists and is not a symlink; remove it manually to enable backtests/latest",
                latest.display()
            );
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }

    #[cfg(unix)]
    std::os::unix::fs::symlink(dir_name, &latest)?;

    #[cfg(all(windows, not(unix)))]
    std::os::windows::fs::symlink_dir(dir_name, &latest)?;

    Ok(())
}

pub(crate) fn validate_strategy_path(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("Strategy file not found: {}", path.display());
    }
    Ok(())
}

pub(crate) fn provider_args(
    data_root: PathBuf,
    data_store: Option<Arc<IcebergStore>>,
) -> providers::ProviderArgs {
    providers::ProviderArgs {
        data_root,
        data_store,
    }
}

pub(crate) fn build_providers(
    args: &RunArgs,
    data_store: Arc<IcebergStore>,
) -> Result<Option<Arc<dyn IHistoryProvider>>> {
    let names = match args.data_provider_historical.as_deref() {
        Some(n) => n,
        None => return Ok(None),
    };

    let raw = providers::build_history_provider(
        names,
        provider_args(args.data.clone(), Some(data_store)),
    )?;
    Ok(Some(raw))
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
    fn test_strategy_name_rust_plugin() {
        let p = Path::new("plugins/my_strategy.so");
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

    #[test]
    fn test_backtest_dir_name_format() {
        use chrono::{TimeZone, Utc};
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 0).unwrap();
        let dir = backtest_dir_name(dt, "sma_crossover");
        assert_eq!(dir, "2026-04-10_143000_sma_crossover");
    }

    #[test]
    fn test_backtest_dir_name_seconds_unique() {
        use chrono::{TimeZone, Utc};
        let dt1 = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 0).unwrap();
        let dt2 = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 5).unwrap();
        let d1 = backtest_dir_name(dt1, "spy_wheel");
        let d2 = backtest_dir_name(dt2, "spy_wheel");
        assert_ne!(d1, d2, "runs on same day must produce different dirs");
    }

    #[test]
    fn test_backtest_dir_name_date_prefix() {
        use chrono::{TimeZone, Utc};
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 9, 5, 3).unwrap();
        let dir = backtest_dir_name(dt, "sma_crossover");
        assert!(dir.starts_with("2026-04-10_090503_"), "dir={dir}");
    }

    #[test]
    fn test_reserve_backtest_dir_adds_suffix_on_collision() {
        use chrono::{TimeZone, Utc};
        let root =
            std::env::temp_dir().join(format!("rlean-backtest-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 0).unwrap();

        let first = reserve_backtest_dir(&root, dt, "strategy").unwrap();
        let second = reserve_backtest_dir(&root, dt, "strategy").unwrap();

        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("2026-04-10_143000_strategy")
        );
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("2026-04-10_143000_strategy_2")
        );
        assert!(first.is_dir());
        assert!(second.is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_update_backtests_latest_symlink() {
        let root =
            std::env::temp_dir().join(format!("rlean-backtest-latest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let first = root.join("2026-06-24_120000_strategy");
        let second = root.join("2026-06-24_120100_strategy");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();

        update_backtests_latest_symlink(&root, &first).unwrap();
        let latest = root.join("latest");
        assert!(latest.is_symlink());
        assert_eq!(
            std::fs::read_link(&latest).unwrap(),
            PathBuf::from("2026-06-24_120000_strategy")
        );
        assert_eq!(
            latest.canonicalize().unwrap(),
            first.canonicalize().unwrap()
        );

        update_backtests_latest_symlink(&root, &second).unwrap();
        assert_eq!(
            std::fs::read_link(&latest).unwrap(),
            PathBuf::from("2026-06-24_120100_strategy")
        );
        assert_eq!(
            latest.canonicalize().unwrap(),
            second.canonicalize().unwrap()
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
