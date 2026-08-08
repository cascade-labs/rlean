use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
#[cfg(not(unix))]
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::cli::{LiveArgs, LiveStatusFilter, LiveSubcommand, RunArgs};
use crate::config::GlobalConfig;
use crate::container_runtime::{self, LiveRunSpec};
use crate::live::is_paper_brokerage_name;
use crate::runtime::{
    backtest_dir_name, resolve_strategy_file, strategy_name_from_path, validate_strategy_path,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LiveDeploymentMetadata {
    deploy_id: String,
    strategy: PathBuf,
    strategy_name: String,
    deployment_dir: PathBuf,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    container_id: Option<String>,
    #[serde(default)]
    container_name: Option<String>,
    #[serde(default)]
    image: Option<String>,
    /// Stable Verglas run catalog id (`live-<deploy_id>`).
    #[serde(default)]
    run_id: Option<String>,
    status: String,
    launched: String,
    stopped: Option<String>,
    updated_at: String,
    brokerage: Option<String>,
    #[serde(default)]
    brokerage_account: Option<String>,
    paper_trading: bool,
    /// Engine argv stored for resume (without the docker wrapper).
    command: Vec<String>,
    error: Option<String>,
    exit_code: Option<i32>,
}

pub(crate) fn launch_live_detached(args: LiveArgs) -> Result<()> {
    let pull = args.pull;
    let mut run_args = args.to_run_args()?;
    run_args.strategy = resolve_strategy_file(run_args.strategy)?;
    if let Ok(canonical) = std::fs::canonicalize(&run_args.strategy) {
        run_args.strategy = canonical;
    }
    validate_strategy_path(&run_args.strategy)?;

    let requested_brokerage = run_args
        .brokerage
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("live mode requires --brokerage paper or a native execution brokerage")
        })?;
    let paper_trading = is_paper_brokerage_name(requested_brokerage);
    let brokerage_account = run_args
        .brokerage_account
        .as_deref()
        .map(str::trim)
        .filter(|account| !account.is_empty())
        .map(str::to_string);
    if paper_trading && brokerage_account.is_some() {
        bail!("--brokerage-account cannot be used with the internal paper brokerage");
    }

    let strategy_name = strategy_name_from_path(&run_args.strategy);
    let live_root = run_args
        .strategy
        .parent()
        .map(|p| p.join("live"))
        .unwrap_or_else(|| PathBuf::from("live"));
    let deployment_dir = reserve_live_dir(&live_root, chrono::Utc::now(), &strategy_name)?;
    let source_strategy = run_args.strategy.clone();
    let deployment_strategy = snapshot_strategy_code(&source_strategy, &deployment_dir)?;
    let mut child_run_args = run_args.clone();
    child_run_args.strategy = deployment_strategy.clone();

    let deploy_id = deployment_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("deployment")
        .to_string();
    let engine_args = live_engine_args(&child_run_args);
    let image = container_runtime::default_image();
    let container_name = container_runtime::live_container_name(&deploy_id);

    let now = chrono::Utc::now().to_rfc3339();
    let initial_metadata = LiveDeploymentMetadata {
        deploy_id: deploy_id.clone(),
        strategy: source_strategy,
        strategy_name,
        deployment_dir: deployment_dir.clone(),
        pid: None,
        container_id: None,
        container_name: Some(container_name.clone()),
        image: Some(image.clone()),
        run_id: Some(format!("live-{deploy_id}")),
        status: "launching".to_string(),
        launched: now.clone(),
        stopped: None,
        updated_at: now,
        brokerage: run_args.brokerage.clone(),
        brokerage_account,
        paper_trading,
        command: engine_args.clone(),
        error: None,
        exit_code: None,
    };
    write_live_deployment_metadata(&deployment_dir, &initial_metadata)?;

    let log_path = deployment_dir.join("live.log");
    File::create(&log_path).with_context(|| format!("failed to create {}", log_path.display()))?;
    register_live_deployment(&deployment_dir);

    let global = GlobalConfig::load()?;
    let container_id = container_runtime::run_live_container(LiveRunSpec {
        image: &image,
        pull,
        deploy_id: &deploy_id,
        deployment_dir: &deployment_dir,
        strategy_file: &deployment_strategy,
        engine_args,
        global: &global,
    })?;
    confirm_live_container_started(&deployment_dir, &container_name, Duration::from_secs(15))?;

    println!(
        "Live deployment started: deploy_id={} container={} dir={}",
        deploy_id,
        container_id,
        deployment_dir.display()
    );
    Ok(())
}

pub(crate) fn run_live_control(command: LiveSubcommand) -> Result<()> {
    match command {
        LiveSubcommand::List { status } => list_live_deployments(status),
        LiveSubcommand::Status { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            let metadata =
                normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
            let effective_status = effective_live_status(&metadata);
            let container_alive = live_container_alive(&metadata);
            let payload = serde_json::json!({
                "deploy_id": metadata.deploy_id,
                "status": effective_status,
                "recorded_status": metadata.status,
                "container_alive": container_alive,
                "container_id": metadata.container_id,
                "container_name": metadata.container_name,
                "image": metadata.image,
                "strategy": metadata.strategy,
                "strategy_name": metadata.strategy_name,
                "deployment_dir": metadata.deployment_dir,
                "launched": metadata.launched,
                "stopped": metadata.stopped,
                "updated_at": metadata.updated_at,
                "brokerage": metadata.brokerage,
                "brokerage_account": metadata.brokerage_account,
                "paper_trading": metadata.paper_trading,
                "run_id": metadata
                    .run_id
                    .clone()
                    .unwrap_or_else(|| format!("live-{}", metadata.deploy_id)),
                "error": metadata.error,
                "exit_code": metadata.exit_code,
                "files": {
                    "log": dir.join("live.log"),
                    "deployment": dir.join("deployment.json")
                }
            });
            print_json_value(&payload)
        }
        LiveSubcommand::Portfolio { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            let metadata = read_live_deployment_metadata(&dir)?;
            let run_id = metadata
                .run_id
                .unwrap_or_else(|| format!("live-{deploy_id}"));
            print_catalog_checkpoint_field(&run_id, "portfolio")
        }
        LiveSubcommand::Orders { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            let metadata = read_live_deployment_metadata(&dir)?;
            let run_id = metadata
                .run_id
                .unwrap_or_else(|| format!("live-{deploy_id}"));
            print_catalog_checkpoint_field(&run_id, "open_orders")
        }
        LiveSubcommand::Logs { deploy_id, lines } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            let metadata = read_live_deployment_metadata(&dir)?;
            let name = metadata
                .container_name
                .unwrap_or_else(|| container_runtime::live_container_name(&deploy_id));
            if container_runtime::container_is_running(&name) || docker_container_exists(&name) {
                container_runtime::print_container_logs(&name, lines)
            } else {
                print_tail(&dir.join("live.log"), lines)
            }
        }
        LiveSubcommand::Pause {
            deploy_id,
            timeout_seconds,
        } => pause_live_deployment(&deploy_id, Duration::from_secs(timeout_seconds)),
        LiveSubcommand::Resume { deploy_id } => resume_live_deployment(&deploy_id),
        LiveSubcommand::Upgrade { deploy_id } => upgrade_live_deployment(&deploy_id),
        LiveSubcommand::Remove { deploy_id, force } => remove_live_deployment(&deploy_id, force),
    }
}

fn pause_live_deployment(deploy_id: &str, timeout: Duration) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let mut metadata =
        normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status == "paused" {
        println!(
            "Live deployment already paused: deploy_id={}",
            metadata.deploy_id
        );
        return Ok(());
    }
    if effective_status != "running" {
        bail!("live deployment {deploy_id} is not running; current status is {effective_status}");
    }

    let name = metadata
        .container_name
        .clone()
        .unwrap_or_else(|| container_runtime::live_container_name(&metadata.deploy_id));
    container_runtime::stop_container(&name, timeout.as_secs())?;

    metadata.pid = None;
    metadata.container_id = None;
    metadata.status = "paused".to_string();
    metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;

    println!(
        "Live deployment paused: deploy_id={} container={}",
        metadata.deploy_id, name
    );
    Ok(())
}

fn resume_live_deployment(deploy_id: &str) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let mut metadata =
        normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status == "running" {
        bail!("live deployment {deploy_id} is already running");
    }
    if metadata.command.is_empty() {
        bail!("live deployment {deploy_id} has no stored command to resume");
    }

    let deployment_strategy = deployment_strategy_path(&metadata, &dir);
    if !deployment_strategy.exists() {
        bail!(
            "live deployment {deploy_id} is missing strategy snapshot at {}",
            deployment_strategy.display()
        );
    }
    write_live_deployment_metadata(&dir, &metadata)?;

    let image = metadata
        .image
        .clone()
        .unwrap_or_else(container_runtime::default_image);
    let global = GlobalConfig::load()?;
    let container_id = container_runtime::run_live_container(LiveRunSpec {
        image: &image,
        pull: false,
        deploy_id: &metadata.deploy_id,
        deployment_dir: &dir,
        strategy_file: &deployment_strategy,
        engine_args: metadata.command.clone(),
        global: &global,
    })?;
    let name = container_runtime::live_container_name(&metadata.deploy_id);
    register_live_deployment(&dir);
    metadata = confirm_live_container_started(&dir, &name, Duration::from_secs(15))?;

    println!(
        "Live deployment resumed: deploy_id={} container={} dir={}",
        metadata.deploy_id,
        container_id,
        dir.display()
    );
    Ok(())
}

fn upgrade_live_deployment(deploy_id: &str) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let mut metadata =
        normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if !matches!(effective_status.as_str(), "paused" | "stopped") {
        bail!(
            "live deployment {deploy_id} must be paused or stopped before upgrade; current status is {effective_status}"
        );
    }

    let image = container_runtime::default_image();
    container_runtime::ensure_image(&image, true)?;
    let deployment_strategy = snapshot_strategy_code(&metadata.strategy, &dir)?;
    metadata.image = Some(image);
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;

    println!(
        "Live deployment upgraded: deploy_id={} image={} code={}",
        metadata.deploy_id,
        metadata.image.as_deref().unwrap_or("-"),
        deployment_strategy.display()
    );
    Ok(())
}

fn remove_live_deployment(deploy_id: &str, force: bool) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let metadata = normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    let name = metadata
        .container_name
        .clone()
        .unwrap_or_else(|| container_runtime::live_container_name(&metadata.deploy_id));
    if effective_status == "running" {
        if !force {
            bail!("live deployment {deploy_id} is running; pause it first or pass --force");
        }
        container_runtime::stop_container(&name, 30)?;
    }
    container_runtime::remove_container(&name)?;

    unregister_live_deployment(&dir);
    std::fs::remove_dir_all(&dir).with_context(|| {
        format!(
            "failed to remove live deployment directory {}",
            dir.display()
        )
    })?;
    println!(
        "Live deployment removed: deploy_id={} dir={}",
        metadata.deploy_id,
        dir.display()
    );
    Ok(())
}

fn list_live_deployments(status_filter: Option<LiveStatusFilter>) -> Result<()> {
    let mut rows = discover_live_deployments()
        .into_iter()
        .filter_map(|dir| {
            read_live_deployment_metadata(&dir).ok().map(|metadata| {
                let normalized = normalize_live_deployment_metadata(&dir, metadata);
                (dir, normalized)
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|(_, a), (_, b)| b.launched.cmp(&a.launched));

    let filter = status_filter.map(|status| status.as_str().to_string());
    println!(
        "{:<36} {:<14} {:<12} {:<24} {:<16} {:<12} STRATEGY",
        "DEPLOY ID", "STATUS", "CONTAINER", "LAUNCHED", "BROKERAGE", "ACCOUNT"
    );
    for (_, metadata) in rows {
        let status = effective_live_status(&metadata);
        if filter.as_ref().is_some_and(|wanted| wanted != &status) {
            continue;
        }
        let container = if live_container_alive(&metadata) {
            "up"
        } else {
            "-"
        };
        println!(
            "{:<36} {:<14} {:<12} {:<24} {:<16} {:<12} {}",
            metadata.deploy_id,
            status,
            container,
            metadata.launched,
            metadata.brokerage.as_deref().unwrap_or("Paper"),
            metadata
                .brokerage_account
                .as_deref()
                .map(mask_brokerage_account)
                .unwrap_or_else(|| "-".to_string()),
            metadata.strategy.display()
        );
    }
    Ok(())
}

impl LiveStatusFilter {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            LiveStatusFilter::Running => "running",
            LiveStatusFilter::Restarting => "restarting",
            LiveStatusFilter::Paused => "paused",
            LiveStatusFilter::Stopped => "stopped",
            LiveStatusFilter::RuntimeError => "runtime-error",
            LiveStatusFilter::Liquidated => "liquidated",
        }
    }
}

fn live_engine_args(args: &RunArgs) -> Vec<String> {
    container_runtime::runtime_engine_args(
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
    )
}

fn live_dir_name(datetime: chrono::DateTime<chrono::Utc>, strategy_name: &str) -> String {
    backtest_dir_name(datetime, strategy_name)
}

fn mask_brokerage_account(account: &str) -> String {
    let visible = account.chars().rev().take(4).collect::<Vec<_>>();
    let suffix = visible.into_iter().rev().collect::<String>();
    format!("****{suffix}")
}

fn reserve_live_dir(
    live_root: &Path,
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(live_root)?;
    let base = live_dir_name(datetime, strategy_name);
    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}_{}", attempt + 1)
        };
        let candidate = live_root.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!(
        "could not reserve unique live directory under {} for {}",
        live_root.display(),
        base
    )
}

fn snapshot_strategy_code(strategy: &Path, deployment_dir: &Path) -> Result<PathBuf> {
    let code_dir = deployment_dir.join("code");
    std::fs::create_dir_all(&code_dir)
        .with_context(|| format!("failed to create {}", code_dir.display()))?;
    let dest = code_dir.join(
        strategy
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("main.py")),
    );
    std::fs::copy(strategy, &dest).with_context(|| {
        format!(
            "failed to copy {} to {}",
            strategy.display(),
            dest.display()
        )
    })?;
    Ok(dest)
}

fn deployment_strategy_path(metadata: &LiveDeploymentMetadata, dir: &Path) -> PathBuf {
    dir.join("code").join(
        metadata
            .strategy
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("main.py")),
    )
}

fn deployment_metadata_path(dir: &Path) -> PathBuf {
    dir.join("deployment.json")
}

fn write_live_deployment_metadata(dir: &Path, metadata: &LiveDeploymentMetadata) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = deployment_metadata_path(dir);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(metadata)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_live_deployment_metadata(dir: &Path) -> Result<LiveDeploymentMetadata> {
    let path = deployment_metadata_path(dir);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn confirm_live_container_started(
    dir: &Path,
    container_name: &str,
    timeout: Duration,
) -> Result<LiveDeploymentMetadata> {
    let deadline = Instant::now() + timeout;
    loop {
        if container_runtime::container_is_running(container_name) {
            let mut metadata = read_live_deployment_metadata(dir)?;
            metadata.container_name = Some(container_name.to_owned());
            metadata.status = "running".to_string();
            metadata.stopped = None;
            metadata.updated_at = chrono::Utc::now().to_rfc3339();
            metadata.error = None;
            metadata.exit_code = None;
            if let Ok(output) = ProcessCommand::new("docker")
                .args(["inspect", "-f", "{{.Id}}", container_name])
                .output()
            {
                if output.status.success() {
                    let id = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    if !id.is_empty() {
                        metadata.container_id = Some(id);
                    }
                }
            }
            write_live_deployment_metadata(dir, &metadata)?;
            return Ok(metadata);
        }

        if Instant::now() >= deadline {
            bail!(
                "live deployment container '{container_name}' did not become running within {}s",
                timeout.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub(crate) fn update_live_deployment_status(
    dir: &Path,
    status: &str,
    error: Option<String>,
    exit_code: Option<i32>,
    _pid: Option<u32>,
) {
    let Ok(mut metadata) = read_live_deployment_metadata(dir) else {
        return;
    };
    metadata.status = status.to_string();
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    if status == "running" {
        metadata.error = None;
        metadata.exit_code = None;
        metadata.stopped = None;
    }
    if let Some(error) = error {
        metadata.error = Some(error);
    }
    if let Some(exit_code) = exit_code {
        metadata.exit_code = Some(exit_code);
    }
    if is_terminal_live_status(status) {
        metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
        metadata.container_id = None;
    }
    let _ = write_live_deployment_metadata(dir, &metadata);
}

fn is_terminal_live_status(status: &str) -> bool {
    matches!(status, "stopped" | "runtime-error" | "liquidated")
}

fn live_container_alive(metadata: &LiveDeploymentMetadata) -> bool {
    let name = metadata
        .container_name
        .clone()
        .unwrap_or_else(|| container_runtime::live_container_name(&metadata.deploy_id));
    container_runtime::container_is_running(&name)
}

fn docker_container_exists(name_or_id: &str) -> bool {
    ProcessCommand::new("docker")
        .args(["inspect", name_or_id])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn effective_live_status(metadata: &LiveDeploymentMetadata) -> String {
    if matches!(metadata.status.as_str(), "running" | "launching") {
        if live_container_alive(metadata) {
            return "running".to_string();
        }
        return "stopped".to_string();
    }
    metadata.status.clone()
}

fn normalize_live_deployment_metadata(
    dir: &Path,
    mut metadata: LiveDeploymentMetadata,
) -> LiveDeploymentMetadata {
    let effective_status = effective_live_status(&metadata);
    let should_update_status = metadata.status != effective_status;
    if should_update_status {
        metadata.status = effective_status;
        if metadata.status != "running" {
            metadata.container_id = None;
            if metadata.stopped.is_none() {
                metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
            }
        }
        metadata.updated_at = chrono::Utc::now().to_rfc3339();
        let _ = write_live_deployment_metadata(dir, &metadata);
    }
    metadata
}

fn register_live_deployment(dir: &Path) {
    let Some(path) = live_registry_path() else {
        return;
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")));
    let mut dirs = read_live_registry();
    dirs.insert(dir.to_path_buf());
    if let Ok(json) = serde_json::to_string_pretty(
        &dirs
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
    ) {
        let _ = std::fs::write(path, json);
    }
}

fn unregister_live_deployment(dir: &Path) {
    let Some(path) = live_registry_path() else {
        return;
    };
    let mut dirs = read_live_registry();
    dirs.remove(dir);
    if let Ok(canonical) = std::fs::canonicalize(dir) {
        dirs.remove(&canonical);
    }
    if let Ok(json) = serde_json::to_string_pretty(
        &dirs
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
    ) {
        let _ = std::fs::write(path, json);
    }
}

fn read_live_registry() -> BTreeSet<PathBuf> {
    let Some(path) = live_registry_path() else {
        return BTreeSet::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn live_registry_path() -> Option<PathBuf> {
    let home = std::env::var_os("RLEAN_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rlean")))?;
    Some(home.join("live").join("registry.json"))
}

fn discover_live_deployments() -> Vec<PathBuf> {
    let mut dirs = read_live_registry();
    if let Ok(current_dir) = std::env::current_dir() {
        collect_live_deployments_under(&current_dir, &mut dirs);
        if let Ok(entries) = std::fs::read_dir(&current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_live_deployments_under(&path, &mut dirs);
                }
            }
        }
    }
    dirs.into_iter()
        .filter(|dir| deployment_metadata_path(dir).exists())
        .collect()
}

fn collect_live_deployments_under(root: &Path, dirs: &mut BTreeSet<PathBuf>) {
    let live_root = root.join("live");
    let Ok(entries) = std::fs::read_dir(live_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if deployment_metadata_path(&path).exists() {
            dirs.insert(path);
        }
    }
}

fn find_live_deployment_dir(deploy_id: &str) -> Result<PathBuf> {
    let path = PathBuf::from(deploy_id);
    if deployment_metadata_path(&path).exists() {
        return Ok(path);
    }
    for dir in discover_live_deployments() {
        if dir.file_name().and_then(|name| name.to_str()) == Some(deploy_id) {
            return Ok(dir);
        }
        if let Ok(metadata) = read_live_deployment_metadata(&dir) {
            if metadata.deploy_id == deploy_id {
                return Ok(dir);
            }
        }
    }
    bail!("live deployment not found: {deploy_id}")
}

fn print_json_value(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_catalog_checkpoint_field(run_id: &str, field: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create tokio runtime for catalog query")?;
    runtime.block_on(async {
        let config = GlobalConfig::load()?;
        let client = crate::runtime::connect_verglas(&config).await?;
        let escaped = run_id.replace('\'', "''");
        let sql = format!(
            "SELECT payload_json FROM rlean.checkpoints \
             WHERE run_id = '{escaped}' ORDER BY recorded_at DESC LIMIT 1"
        );
        use futures::TryStreamExt;
        let batches = client
            .query_stream(&sql)
            .await
            .context("query live checkpoint")?
            .try_collect::<Vec<_>>()
            .await
            .context("read live checkpoint stream")?;
        let payload = batches
            .iter()
            .flat_map(|batch| {
                use arrow_array::Array;
                let col = batch.column(0);
                (0..batch.num_rows()).filter_map(move |row| {
                    if col.is_null(row) {
                        None
                    } else {
                        arrow_cast::display::array_value_to_string(col.as_ref(), row).ok()
                    }
                })
            })
            .next()
            .ok_or_else(|| anyhow::anyhow!("no catalog checkpoint for run_id={run_id}"))?;
        let value: serde_json::Value =
            serde_json::from_str(&payload).context("parse checkpoint payload")?;
        let field_value = value.get(field).cloned().unwrap_or(serde_json::Value::Null);
        print_json_value(&field_value)
    })
}

fn print_tail(path: &Path, lines: usize) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if lines == 0 {
        return Ok(());
    }
    let all_lines = text.lines().collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LiveLimitArgs, RuntimeArgs};

    #[test]
    fn test_reserve_live_dir_adds_suffix_on_collision() {
        use chrono::{TimeZone, Utc};

        let root = std::env::temp_dir().join(format!("rlean-live-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dt = Utc.with_ymd_and_hms(2026, 6, 13, 14, 30, 0).unwrap();

        let first = reserve_live_dir(&root, dt, "strategy").unwrap();
        let second = reserve_live_dir(&root, dt, "strategy").unwrap();

        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("2026-06-13_143000_strategy")
        );
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("2026-06-13_143000_strategy_2")
        );
        assert!(first.is_dir());
        assert!(second.is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_live_engine_args_include_runtime_flags() {
        let args = RunArgs {
            strategy: PathBuf::from("/tmp/strategy/main.py"),
            runtime: RuntimeArgs {
                data_provider_historical: Some("massive".to_string()),
                live_data_feed: Some("tradier".to_string()),
                brokerage: Some("http".to_string()),
                brokerage_url: Some("http://127.0.0.1:5199".to_string()),
                brokerage_account: Some("account-1234".to_string()),
                start_date: None,
                end_date: None,
                parameters: vec!["foo=bar".to_string()],
                verbose: true,
            },
            live_limits: LiveLimitArgs {
                live_max_slices: Some(2),
                live_max_runtime_seconds: Some(10),
            },
            native: false,
            pull: false,
        };
        let engine_args = live_engine_args(&args);

        assert!(engine_args.contains(&"--parameter".to_string()));
        assert!(engine_args.contains(&"foo=bar".to_string()));
        assert!(engine_args.contains(&"--data-provider-historical".to_string()));
        assert!(engine_args.contains(&"massive".to_string()));
        assert!(engine_args.contains(&"--live-data-feed".to_string()));
        assert!(engine_args.contains(&"--brokerage-url".to_string()));
        assert!(engine_args.contains(&"http://host.docker.internal:5199".to_string()));
        assert!(engine_args.contains(&"--brokerage-account".to_string()));
        assert!(engine_args.contains(&"account-1234".to_string()));
        assert!(engine_args.contains(&"-v".to_string()));
    }

    #[test]
    fn test_metadata_round_trips_container_fields() {
        let root = std::env::temp_dir().join(format!(
            "rlean-live-container-meta-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let meta = LiveDeploymentMetadata {
            deploy_id: "deploy-1".to_string(),
            strategy: PathBuf::from("/tmp/source/main.py"),
            strategy_name: "source".to_string(),
            deployment_dir: root.clone(),
            pid: None,
            container_id: Some("abc123".into()),
            container_name: Some("rlean-live-deploy-1".into()),
            image: Some(container_runtime::DEFAULT_IMAGE.into()),
            run_id: Some("live-deploy-1".into()),
            status: "paused".to_string(),
            launched: "2026-06-22T00:00:00Z".to_string(),
            stopped: Some("2026-06-22T00:01:00Z".to_string()),
            updated_at: "2026-06-22T00:01:00Z".to_string(),
            brokerage: Some("paper".to_string()),
            brokerage_account: None,
            paper_trading: true,
            command: vec!["--brokerage".into(), "paper".into()],
            error: None,
            exit_code: None,
        };
        write_live_deployment_metadata(&root, &meta).unwrap();
        let persisted = read_live_deployment_metadata(&root).unwrap();
        assert_eq!(persisted.container_id.as_deref(), Some("abc123"));
        assert_eq!(
            persisted.container_name.as_deref(),
            Some("rlean-live-deploy-1")
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
