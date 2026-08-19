//! Docker-backed runtime for backtest and live.
//!
//! The host `rlean` CLI never runs the engine natively for those modes; it
//! validates Docker + Verglas, ensures the image, and spawns containers.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};

use crate::config::{self, GlobalConfig};

/// Default GHCR image tip of main. Override with `RLEAN_IMAGE`.
pub(crate) const DEFAULT_IMAGE: &str = "ghcr.io/cascade-labs/rlean:latest";

pub(crate) fn default_image() -> String {
    std::env::var("RLEAN_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_owned())
}

pub(crate) fn live_container_name(deploy_id: &str) -> String {
    format!("rlean-live-{deploy_id}")
}

pub(crate) fn ensure_environment(global: &GlobalConfig) -> Result<()> {
    ensure_docker()?;
    // A machine with no Verglas configured runs the engine without a cache,
    // custom-data feed, or run catalog. There is no gateway to preflight.
    if let Some(settings) = config::verglas_settings(global)? {
        ensure_verglas(&settings)?;
    }
    Ok(())
}

pub(crate) fn ensure_docker() -> Result<()> {
    let output = docker_output(["info"])?;
    if !output.status.success() {
        bail!(
            "Docker is required but `docker info` failed: {}",
            stderr_tail(&output)
        );
    }
    Ok(())
}

pub(crate) fn ensure_verglas(settings: &config::VerglasSettings) -> Result<()> {
    let endpoint = settings.endpoint.as_str();
    let health_url = format!("{}/health", endpoint.trim_end_matches('/'));
    // Prefer curl when present; fall back to a TCP connect on the URL host/port.
    let curl = Command::new("curl")
        .args(["-fsS", "--max-time", "5", &health_url])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status();
    match curl {
        Ok(status) if status.success() => return Ok(()),
        Ok(_) | Err(_) => {}
    }
    // Gateway may not expose /health; accept any HTTP response from the root.
    let root = endpoint.trim_end_matches('/').to_owned();
    let probe = Command::new("curl")
        .args([
            "-fsS",
            "--max-time",
            "5",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &root,
        ])
        .output();
    if let Ok(output) = probe {
        let code = String::from_utf8_lossy(&output.stdout);
        if output.status.success()
            || code.starts_with('2')
            || code.starts_with('3')
            || code.starts_with('4')
        {
            // 4xx still means the daemon answered.
            return Ok(());
        }
    }
    bail!(
        "Verglas gateway is not reachable at {endpoint}. Start the Verglas daemon \
         (e.g. `verglas status`) and set verglas_endpoint / VERGLAS_ENDPOINT."
    )
}

/// Pull and/or verify the engine image is present.
///
/// When `pull` is true (cloud deploy/upgrade), always `docker pull` and fail on
/// error. Otherwise pull only when the local image tag is missing.
pub(crate) fn ensure_image(image: &str, pull: bool) -> Result<()> {
    if pull || !image_present(image)? {
        let output = docker_output(["pull", image])?;
        if !output.status.success() {
            bail!(
                "failed to pull container image '{image}': {}",
                stderr_tail(&output)
            );
        }
    }
    Ok(())
}

pub(crate) fn image_present(image: &str) -> Result<bool> {
    let output = docker_output(["image", "inspect", image])?;
    Ok(output.status.success())
}

pub(crate) struct BacktestRunSpec<'a> {
    pub image: &'a str,
    pub pull: bool,
    pub strategy_file: &'a Path,
    pub engine_args: Vec<String>,
    pub global: &'a GlobalConfig,
}

/// Run a finite backtest container; stream stdout/stderr; return exit code.
pub(crate) fn run_backtest_container(spec: BacktestRunSpec<'_>) -> Result<i32> {
    ensure_environment(spec.global)?;
    ensure_image(spec.image, spec.pull)?;

    let strategy_file = std::fs::canonicalize(spec.strategy_file)
        .with_context(|| format!("canonicalize {}", spec.strategy_file.display()))?;
    let strategy_dir = strategy_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("strategy file has no parent directory"))?;
    let strategy_name = strategy_file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid strategy file name"))?;
    let config_file = write_container_config(spec.global)?;

    let mut args = base_run_args(spec.image, spec.global)?;
    args.push("--rm".into());
    args.extend([
        "-v".into(),
        format!("{}:/strategy:ro", strategy_dir.display()),
    ]);
    args.extend([
        "-v".into(),
        format!("{}:/root/.rlean/config:ro", config_file.display()),
    ]);
    args.push(spec.image.to_owned());
    args.push("backtest".into());
    args.push("--native".into());
    args.push(format!("/strategy/{strategy_name}"));
    args.extend(spec.engine_args);

    let status = Command::new("docker")
        .args(&args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("failed to spawn docker for backtest")?;
    Ok(status.code().unwrap_or(1))
}

pub(crate) struct LiveRunSpec<'a> {
    pub image: &'a str,
    pub pull: bool,
    pub deploy_id: &'a str,
    pub deployment_dir: &'a Path,
    /// Host path to the strategy file that the container should execute
    /// (usually the deployment code snapshot).
    pub strategy_file: &'a Path,
    pub engine_args: Vec<String>,
    pub global: &'a GlobalConfig,
}

/// Start a detached live container. Returns the container id.
pub(crate) fn run_live_container(spec: LiveRunSpec<'_>) -> Result<String> {
    ensure_environment(spec.global)?;
    ensure_image(spec.image, spec.pull)?;

    let name = live_container_name(spec.deploy_id);
    // Replace any leftover container with the same name.
    let _ = docker_output(["rm", "-f", &name]);

    let strategy_file = std::fs::canonicalize(spec.strategy_file)
        .with_context(|| format!("canonicalize {}", spec.strategy_file.display()))?;
    let strategy_dir = strategy_file
        .parent()
        .ok_or_else(|| anyhow::anyhow!("strategy file has no parent directory"))?;
    let strategy_name = strategy_file
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid strategy file name"))?;
    let deployment_dir = std::fs::canonicalize(spec.deployment_dir)
        .with_context(|| format!("canonicalize {}", spec.deployment_dir.display()))?;
    let config_file = write_container_config(spec.global)?;

    let mut args = base_run_args(spec.image, spec.global)?;
    args.extend([
        "-d".into(),
        "--restart".into(),
        "unless-stopped".into(),
        "--name".into(),
        name.clone(),
        "-v".into(),
        format!("{}:/strategy:ro", strategy_dir.display()),
        "-v".into(),
        format!("{}:/data/live/{}", deployment_dir.display(), spec.deploy_id),
    ]);
    args.extend([
        "-v".into(),
        format!("{}:/root/.rlean/config:ro", config_file.display()),
    ]);
    args.push(spec.image.to_owned());
    args.push("live".into());
    args.push("--foreground".into());
    args.push("--live-deploy-dir".into());
    args.push(format!("/data/live/{}", spec.deploy_id));
    args.push(format!("/strategy/{strategy_name}"));
    args.extend(spec.engine_args);

    let output = docker_output(args.iter().map(String::as_str).collect::<Vec<_>>())?;
    if !output.status.success() {
        bail!(
            "failed to start live container '{name}': {}",
            stderr_tail(&output)
        );
    }
    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if id.is_empty() {
        bail!("docker run returned an empty container id for '{name}'");
    }
    Ok(id)
}

pub(crate) fn stop_container(name_or_id: &str, timeout_seconds: u64) -> Result<()> {
    let output = docker_output(["stop", "-t", &timeout_seconds.to_string(), name_or_id])?;
    if !output.status.success() {
        // Already stopped is fine.
        let err = stderr_tail(&output);
        if !err.contains("No such container") && !err.contains("is not running") {
            bail!("failed to stop container '{name_or_id}': {err}");
        }
    }
    Ok(())
}

pub(crate) fn remove_container(name_or_id: &str) -> Result<()> {
    let output = docker_output(["rm", "-f", name_or_id])?;
    if !output.status.success() {
        let err = stderr_tail(&output);
        if !err.contains("No such container") {
            bail!("failed to remove container '{name_or_id}': {err}");
        }
    }
    Ok(())
}

pub(crate) fn container_is_running(name_or_id: &str) -> bool {
    docker_output(["inspect", "-f", "{{.State.Running}}", name_or_id])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true")
        .unwrap_or(false)
}

pub(crate) fn print_container_logs(name_or_id: &str, lines: usize) -> Result<()> {
    let output = docker_output(["logs", "--tail", &lines.to_string(), name_or_id])?;
    // docker logs writes the container stream to stderr as well; print both.
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    if !output.status.success() {
        bail!(
            "failed to read logs for container '{name_or_id}': {}",
            stderr_tail(&output)
        );
    }
    Ok(())
}

/// Rewrite loopback Verglas URLs so containers reach the host gateway.
pub(crate) fn containerize_loopback_url(url: &str) -> String {
    url.replace("127.0.0.1", "host.docker.internal")
        .replace("localhost", "host.docker.internal")
}

/// Materialize the host configuration for container use. Provider endpoints
/// are machine-local configuration, so loopback URLs must point at the Docker
/// host while credentials and all non-endpoint settings remain unchanged.
fn write_container_config(global: &GlobalConfig) -> Result<PathBuf> {
    let container = containerized_config(global, container_uses_host_network());
    let path = config::config_path()?.with_file_name("container-config");
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(&container)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temporary, &path)?;
    Ok(path)
}

fn containerized_config(global: &GlobalConfig, host_network: bool) -> GlobalConfig {
    let mut container = global.clone();
    if !host_network {
        container.verglas_endpoint = container
            .verglas_endpoint
            .as_deref()
            .map(containerize_loopback_url);
        container.verglas_access_uri = container
            .verglas_access_uri
            .as_deref()
            .map(containerize_loopback_url);
        for provider in container.providers.values_mut() {
            for (key, value) in provider.iter_mut() {
                let key = key.to_ascii_lowercase();
                if key.contains("url") || key.contains("endpoint") {
                    *value = containerize_loopback_url(value);
                }
            }
        }
    }
    // ThetaData's native default is its host-side terminal port. A configured
    // provider with no explicit base URL therefore still needs the Docker host
    // gateway when the engine runs in a container.
    if let Some(thetadata) = container.providers.get_mut("thetadata") {
        thetadata.entry("base_url".to_string()).or_insert_with(|| {
            if host_network {
                "http://127.0.0.1:25510".to_string()
            } else {
                "http://host.docker.internal:25510".to_string()
            }
        });
    }
    container
}

fn container_uses_host_network() -> bool {
    cfg!(target_os = "linux")
}

fn endpoint_for_container(endpoint: &str, host_network: bool) -> String {
    if host_network {
        endpoint.to_owned()
    } else {
        containerize_loopback_url(endpoint)
    }
}

/// `-e VERGLAS_*` flags for the engine container, or nothing at all when this
/// machine has no Verglas: the container then resolves the same absent
/// configuration the host did and runs without a gateway.
fn verglas_run_args(settings: Option<&config::VerglasSettings>, host_network: bool) -> Vec<String> {
    let Some(settings) = settings else {
        return Vec::new();
    };
    let mut args = vec![
        "-e".into(),
        format!(
            "VERGLAS_ENDPOINT={}",
            endpoint_for_container(&settings.endpoint, host_network)
        ),
        "-e".into(),
        format!("VERGLAS_DATABASE={}", settings.database),
    ];
    if let Some(access_uri) = settings.access_uri.as_deref() {
        args.extend([
            "-e".into(),
            format!(
                "VERGLAS_ACCESS_URI={}",
                endpoint_for_container(access_uri, host_network)
            ),
        ]);
    }
    if let Some(token) = settings.token.as_deref() {
        args.extend(["-e".into(), format!("VERGLAS_TOKEN={token}")]);
    }
    args
}

fn base_run_args(image: &str, global: &GlobalConfig) -> Result<Vec<String>> {
    let _ = image;
    let mut args = vec!["run".into()];
    if container_uses_host_network() {
        args.extend(["--network".into(), "host".into()]);
    } else {
        args.extend([
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
        ]);
    }
    args.extend(verglas_run_args(
        config::verglas_settings(global)?.as_ref(),
        container_uses_host_network(),
    ));
    if let Some(api_key) = unusual_whales_api_key(global) {
        args.extend(["-e".into(), format!("UNUSUALWHALES_KEY={api_key}")]);
    }
    Ok(args)
}

fn unusual_whales_api_key(global: &GlobalConfig) -> Option<String> {
    std::env::var("UNUSUALWHALES_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| configured_unusual_whales_api_key(global))
}

fn configured_unusual_whales_api_key(global: &GlobalConfig) -> Option<String> {
    global
        .providers
        .get("unusual_whales")
        .and_then(|provider| provider.get("api_key"))
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

fn docker_output(args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> Result<Output> {
    Command::new("docker")
        .args(args)
        .output()
        .context("failed to execute docker; is Docker installed and on PATH?")
}

fn stderr_tail(output: &Output) -> String {
    let text = String::from_utf8_lossy(&output.stderr);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Build engine argv flags from runtime options (without the subcommand/strategy).
#[allow(clippy::too_many_arguments)]
pub(crate) fn runtime_engine_args(
    data_provider_historical: Option<&str>,
    live_data_feed: Option<&str>,
    brokerage: Option<&str>,
    brokerage_url: Option<&str>,
    brokerage_account: Option<&str>,
    start_date: Option<&str>,
    end_date: Option<&str>,
    parameters: &[String],
    verbose: bool,
    live_max_slices: Option<usize>,
    live_max_runtime_seconds: Option<u64>,
) -> Vec<String> {
    let mut values = Vec::new();
    if let Some(value) = data_provider_historical {
        values.extend(["--data-provider-historical".into(), value.to_owned()]);
    }
    if let Some(value) = live_data_feed {
        values.extend(["--live-data-feed".into(), value.to_owned()]);
    }
    if let Some(value) = brokerage {
        values.extend(["--brokerage".into(), value.to_owned()]);
    }
    if let Some(value) = brokerage_url {
        values.extend(["--brokerage-url".into(), containerize_loopback_url(value)]);
    }
    if let Some(value) = brokerage_account {
        values.extend(["--brokerage-account".into(), value.to_owned()]);
    }
    if let Some(value) = start_date {
        values.extend(["--start-date".into(), value.to_owned()]);
    }
    if let Some(value) = end_date {
        values.extend(["--end-date".into(), value.to_owned()]);
    }
    for parameter in parameters {
        values.extend(["--parameter".into(), parameter.clone()]);
    }
    if verbose {
        values.push("-v".into());
    }
    if let Some(value) = live_max_slices {
        values.extend(["--live-max-slices".into(), value.to_string()]);
    }
    if let Some(value) = live_max_runtime_seconds {
        values.extend(["--live-max-runtime-seconds".into(), value.to_string()]);
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containerize_rewrites_loopback() {
        assert_eq!(
            containerize_loopback_url("http://127.0.0.1:8334"),
            "http://host.docker.internal:8334"
        );
        assert_eq!(
            containerize_loopback_url("http://localhost:5199"),
            "http://host.docker.internal:5199"
        );
    }

    #[test]
    fn container_config_rewrites_provider_endpoints_without_changing_credentials() {
        let mut global = GlobalConfig {
            verglas_endpoint: Some("http://127.0.0.1:8334".to_string()),
            verglas_access_uri: Some("http://127.0.0.1:8345".to_string()),
            ..GlobalConfig::default()
        };
        global.providers.insert(
            "thetadata".to_string(),
            [
                ("api_key".to_string(), "secret".to_string()),
                (
                    "base_url".to_string(),
                    "http://localhost:25510/v3".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
        );

        let container = containerized_config(&global, false);
        assert_eq!(
            container.verglas_endpoint.as_deref(),
            Some("http://host.docker.internal:8334")
        );
        assert_eq!(
            container.verglas_access_uri.as_deref(),
            Some("http://host.docker.internal:8345")
        );
        assert_eq!(
            container.providers["thetadata"]["base_url"],
            "http://host.docker.internal:25510/v3"
        );
        assert_eq!(container.providers["thetadata"]["api_key"], "secret");
    }

    #[test]
    fn container_config_materializes_thetadata_host_default() {
        let mut global = GlobalConfig::default();
        global.providers.insert(
            "thetadata".to_string(),
            [("api_key".to_string(), "secret".to_string())]
                .into_iter()
                .collect(),
        );

        let container = containerized_config(&global, false);
        assert_eq!(
            container.providers["thetadata"]["base_url"],
            "http://host.docker.internal:25510"
        );
    }

    #[test]
    fn host_network_container_config_preserves_loopback_endpoints() {
        let mut global = GlobalConfig {
            verglas_endpoint: Some("http://127.0.0.1:8334".to_string()),
            ..GlobalConfig::default()
        };
        global.providers.insert(
            "thetadata".to_string(),
            [("api_key".to_string(), "secret".to_string())]
                .into_iter()
                .collect(),
        );

        let container = containerized_config(&global, true);
        assert_eq!(
            container.verglas_endpoint.as_deref(),
            Some("http://127.0.0.1:8334")
        );
        assert_eq!(
            container.providers["thetadata"]["base_url"],
            "http://127.0.0.1:25510"
        );
    }

    #[test]
    fn container_verglas_env_rewrites_loopback_endpoints() {
        let settings = config::VerglasSettings {
            endpoint: "http://127.0.0.1:8334".to_string(),
            access_uri: Some("http://127.0.0.1:8345".to_string()),
            database: "rlean".to_string(),
            token: Some("secret".to_string()),
        };

        assert_eq!(
            verglas_run_args(Some(&settings), false),
            vec![
                "-e".to_string(),
                "VERGLAS_ENDPOINT=http://host.docker.internal:8334".to_string(),
                "-e".to_string(),
                "VERGLAS_DATABASE=rlean".to_string(),
                "-e".to_string(),
                "VERGLAS_ACCESS_URI=http://host.docker.internal:8345".to_string(),
                "-e".to_string(),
                "VERGLAS_TOKEN=secret".to_string(),
            ]
        );
        assert_eq!(
            verglas_run_args(Some(&settings), true),
            vec![
                "-e".to_string(),
                "VERGLAS_ENDPOINT=http://127.0.0.1:8334".to_string(),
                "-e".to_string(),
                "VERGLAS_DATABASE=rlean".to_string(),
                "-e".to_string(),
                "VERGLAS_ACCESS_URI=http://127.0.0.1:8345".to_string(),
                "-e".to_string(),
                "VERGLAS_TOKEN=secret".to_string(),
            ]
        );
    }

    #[test]
    fn container_gets_no_verglas_env_when_verglas_is_not_configured() {
        assert!(verglas_run_args(None, false).is_empty());
        assert!(verglas_run_args(None, true).is_empty());
    }

    #[test]
    fn live_container_name_is_stable() {
        assert_eq!(
            live_container_name("2026-08-07_120000_strategy"),
            "rlean-live-2026-08-07_120000_strategy"
        );
    }

    #[test]
    fn unusual_whales_key_comes_from_provider_config() {
        let mut global = GlobalConfig::default();
        global.providers.insert(
            "unusual_whales".to_string(),
            [("api_key".to_string(), "uw-secret".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(
            configured_unusual_whales_api_key(&global).as_deref(),
            Some("uw-secret")
        );
    }
}
