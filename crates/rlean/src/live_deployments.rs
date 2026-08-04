use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
#[cfg(not(unix))]
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use rlean::daemon::{self, DeploymentSpec, Request};
use serde::{Deserialize, Serialize};

use crate::cli::{LiveArgs, LiveStatusFilter, LiveSubcommand, RunArgs};
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
    pid: Option<u32>,
    status: String,
    launched: String,
    stopped: Option<String>,
    updated_at: String,
    brokerage: Option<String>,
    #[serde(default)]
    brokerage_account: Option<String>,
    paper_trading: bool,
    command: Vec<String>,
    error: Option<String>,
    exit_code: Option<i32>,
}

pub(crate) fn launch_live_detached(args: LiveArgs) -> Result<()> {
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
            anyhow::anyhow!("live mode requires --brokerage paper or a sidecar execution brokerage")
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
    let exe = std::env::current_exe().context("failed to resolve current rlean executable")?;
    let child_args = live_child_args(&child_run_args, &deployment_dir);
    let command = std::iter::once(exe.to_string_lossy().to_string())
        .chain(child_args.iter().cloned())
        .collect::<Vec<_>>();

    let now = chrono::Utc::now().to_rfc3339();
    let initial_metadata = LiveDeploymentMetadata {
        deploy_id: deploy_id.clone(),
        strategy: source_strategy,
        strategy_name,
        deployment_dir: deployment_dir.clone(),
        pid: None,
        status: "launching".to_string(),
        launched: now.clone(),
        stopped: None,
        updated_at: now,
        brokerage: run_args.brokerage.clone(),
        brokerage_account,
        paper_trading,
        command,
        error: None,
        exit_code: None,
    };
    write_live_deployment_metadata(&deployment_dir, &initial_metadata)?;

    let log_path = deployment_dir.join("live.log");
    File::create(&log_path).with_context(|| format!("failed to create {}", log_path.display()))?;
    let mut environment = BTreeMap::new();
    if let Some(token) = &run_args.data_sidecar_token {
        environment.insert("RLEAN_DATA_SIDECAR_TOKEN".to_owned(), token.clone());
    }
    register_live_deployment(&deployment_dir);
    let response = daemon::request(&Request::Start(DeploymentSpec {
        deploy_id: deploy_id.clone(),
        deployment_dir: deployment_dir.clone(),
        program: exe,
        args: child_args,
        working_dir: deployment_strategy
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        log_path: log_path.clone(),
        environment,
        desired_state: "running".to_owned(),
        restart: false,
    }))?;
    let pid = response
        .pid
        .ok_or_else(|| anyhow::anyhow!("rleand did not return a child pid"))?;
    confirm_live_deployment_started(&deployment_dir, pid, &log_path, Duration::from_secs(10))?;

    println!(
        "Live deployment started: deploy_id={} pid={} dir={} log={}",
        deploy_id,
        pid,
        deployment_dir.display(),
        log_path.display()
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
            let running_pid = running_live_pid(&metadata);
            let process_alive = running_pid.is_some();
            let payload = serde_json::json!({
                "deploy_id": metadata.deploy_id,
                "status": effective_status,
                "recorded_status": metadata.status,
                "process_alive": process_alive,
                "pid": running_pid,
                "strategy": metadata.strategy,
                "strategy_name": metadata.strategy_name,
                "deployment_dir": metadata.deployment_dir,
                "launched": metadata.launched,
                "stopped": metadata.stopped,
                "updated_at": metadata.updated_at,
                "brokerage": metadata.brokerage,
                "brokerage_account": metadata.brokerage_account,
                "paper_trading": metadata.paper_trading,
                "error": metadata.error,
                "exit_code": metadata.exit_code,
                "files": {
                    "pid": dir.join("pid"),
                    "log": dir.join("live.log"),
                    "portfolio": dir.join("portfolio.json"),
                    "orders": dir.join("orders.json"),
                    "order_events": dir.join("order-events.jsonl"),
                    "trades": dir.join("trades.jsonl"),
                    "progress": dir.join("progress.json"),
                    "deployment": dir.join("deployment.json")
                }
            });
            print_json_value(&payload)
        }
        LiveSubcommand::Portfolio { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            print_json_file(&dir.join("portfolio.json"))
        }
        LiveSubcommand::Orders { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            print_json_file(&dir.join("orders.json"))
        }
        LiveSubcommand::Logs { deploy_id, lines } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            print_tail(&dir.join("live.log"), lines)
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

    let pid = metadata.pid;
    daemon::request(&Request::Stop {
        deploy_id: metadata.deploy_id.clone(),
        timeout_seconds: timeout.as_secs(),
    })?;

    metadata.pid = None;
    metadata.status = "paused".to_string();
    metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;
    let _ = std::fs::remove_file(dir.join("pid"));

    println!(
        "Live deployment paused: deploy_id={} pid={}",
        metadata.deploy_id,
        pid.map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_owned())
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
    if deployment_strategy.exists() {
        rewrite_live_command_strategy(&mut metadata.command, &deployment_strategy);
    }
    // Persist the snapshot strategy path so rleand replays the same command on
    // every later restart.
    write_live_deployment_metadata(&dir, &metadata)?;

    let exe = PathBuf::from(&metadata.command[0]);
    let child_args = metadata.command[1..].to_vec();
    let log_path = dir.join("live.log");
    let working_dir = deployment_strategy
        .parent()
        .or_else(|| metadata.strategy.parent())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let response = daemon::request(&Request::Start(DeploymentSpec {
        deploy_id: metadata.deploy_id.clone(),
        deployment_dir: dir.clone(),
        program: exe,
        args: child_args,
        working_dir,
        log_path: log_path.clone(),
        environment: BTreeMap::new(),
        desired_state: "running".to_owned(),
        restart: true,
    }))?;
    let pid = response
        .pid
        .ok_or_else(|| anyhow::anyhow!("rleand did not return a child pid"))?;
    register_live_deployment(&dir);
    metadata = confirm_live_deployment_started(&dir, pid, &log_path, Duration::from_secs(10))?;

    println!(
        "Live deployment resumed: deploy_id={} pid={} dir={} log={}",
        metadata.deploy_id,
        pid,
        dir.display(),
        log_path.display()
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

    let deployment_strategy = snapshot_strategy_code(&metadata.strategy, &dir)?;
    rewrite_live_command_strategy(&mut metadata.command, &deployment_strategy);
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;

    println!(
        "Live deployment upgraded: deploy_id={} code={}",
        metadata.deploy_id,
        deployment_strategy.display()
    );
    Ok(())
}

fn remove_live_deployment(deploy_id: &str, force: bool) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let metadata = normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status == "running" {
        if !force {
            bail!("live deployment {deploy_id} is running; pause it first or pass --force");
        }
        daemon::request(&Request::Stop {
            deploy_id: metadata.deploy_id.clone(),
            timeout_seconds: 30,
        })?;
    }

    daemon::request(&Request::Remove {
        deploy_id: metadata.deploy_id.clone(),
    })?;

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
        "{:<36} {:<14} {:<8} {:<24} {:<16} {:<12} STRATEGY",
        "DEPLOY ID", "STATUS", "PID", "LAUNCHED", "BROKERAGE", "ACCOUNT"
    );
    for (_, metadata) in rows {
        let status = effective_live_status(&metadata);
        if filter.as_ref().is_some_and(|wanted| wanted != &status) {
            continue;
        }
        println!(
            "{:<36} {:<14} {:<8} {:<24} {:<16} {:<12} {}",
            metadata.deploy_id,
            status,
            running_live_pid(&metadata)
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
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

fn live_child_args(args: &RunArgs, deployment_dir: &Path) -> Vec<String> {
    let mut values = vec![
        "live".to_string(),
        "--foreground".to_string(),
        "--live-deploy-dir".to_string(),
        deployment_dir.to_string_lossy().to_string(),
        args.strategy.to_string_lossy().to_string(),
    ];

    if let Some(value) = &args.data_sidecar {
        values.extend(["--data-sidecar".to_string(), value.clone()]);
    }
    if let Some(value) = &args.data_provider_historical {
        values.extend(["--data-provider-historical".to_string(), value.clone()]);
    }
    if let Some(value) = &args.live_data_feed {
        values.extend(["--live-data-feed".to_string(), value.clone()]);
    }
    if let Some(value) = &args.brokerage {
        values.extend(["--brokerage".to_string(), value.clone()]);
    }
    if let Some(value) = &args.brokerage_account {
        values.extend(["--brokerage-account".to_string(), value.clone()]);
    }
    if let Some(value) = &args.start_date {
        values.extend(["--start-date".to_string(), value.clone()]);
    }
    if let Some(value) = &args.end_date {
        values.extend(["--end-date".to_string(), value.clone()]);
    }
    for parameter in &args.parameters {
        values.extend(["--parameter".to_string(), parameter.clone()]);
    }
    if let Some(value) = args.live_limits.live_max_slices {
        values.extend(["--live-max-slices".to_string(), value.to_string()]);
    }
    if let Some(value) = args.live_limits.live_max_runtime_seconds {
        values.extend(["--live-max-runtime-seconds".to_string(), value.to_string()]);
    }
    // Propagate the artifact relay flags so the detached child mirrors to S3 too.
    if let Some(value) = &args.artifact_store {
        values.extend(["--artifact-store".to_string(), value.clone()]);
    }
    if let Some(value) = &args.artifact_s3 {
        values.extend(["--artifact-s3".to_string(), value.clone()]);
    }
    if args.verbose {
        values.push("--verbose".to_string());
    }
    values
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

fn rewrite_live_command_strategy(command: &mut [String], deployment_strategy: &Path) {
    let Some(last_strategy_arg) = command
        .iter()
        .enumerate()
        .rfind(|(_, arg)| arg.ends_with(".py") || arg.ends_with(".rs"))
        .map(|(index, _)| index)
    else {
        return;
    };
    command[last_strategy_arg] = deployment_strategy.display().to_string();
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

fn write_pid_file(dir: &Path, pid: u32) -> Result<()> {
    std::fs::write(dir.join("pid"), format!("{pid}\n"))?;
    Ok(())
}

#[cfg(test)]
fn mark_live_deployment_running(
    dir: &Path,
    fallback: &LiveDeploymentMetadata,
    pid: u32,
) -> Result<LiveDeploymentMetadata> {
    let mut metadata = read_live_deployment_metadata(dir).unwrap_or_else(|_| fallback.clone());
    if is_terminal_live_status(&metadata.status) && metadata.pid == Some(pid) {
        return Ok(metadata);
    }

    metadata.pid = Some(pid);
    metadata.status = "running".to_string();
    metadata.stopped = None;
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(dir, &metadata)?;
    Ok(metadata)
}

fn confirm_live_deployment_started(
    dir: &Path,
    pid: u32,
    log_path: &Path,
    timeout: Duration,
) -> Result<LiveDeploymentMetadata> {
    let deadline = Instant::now() + timeout;
    loop {
        let metadata = match read_live_deployment_metadata(dir) {
            Ok(metadata) => normalize_live_deployment_metadata(dir, metadata),
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(error) => return Err(error),
        };

        if running_live_pid(&metadata) == Some(pid) {
            return Ok(metadata);
        }

        // The daemon can return the new child pid before that child replaces a
        // previous terminal deployment.json snapshot with `running`. Treat a
        // terminal status as belonging to this launch only after the snapshot
        // carries this exact pid; otherwise keep waiting while the child lives.
        if (is_terminal_live_status(&metadata.status) && metadata.pid == Some(pid))
            || !process_is_alive(pid)
        {
            bail!(
                "live deployment {} exited during startup; status={}{}{}; log={}",
                metadata.deploy_id,
                metadata.status,
                metadata
                    .exit_code
                    .map(|code| format!(" exit_code={code}"))
                    .unwrap_or_default(),
                metadata
                    .error
                    .as_ref()
                    .map(|error| format!(" error={error}"))
                    .unwrap_or_default(),
                log_path.display()
            );
        }

        if Instant::now() >= deadline {
            bail!(
                "live deployment {} did not report running within {}s; pid={pid} log={}",
                metadata.deploy_id,
                timeout.as_secs(),
                log_path.display()
            );
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn update_live_deployment_status(
    dir: &Path,
    status: &str,
    error: Option<String>,
    exit_code: Option<i32>,
    pid: Option<u32>,
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
    if let Some(pid) = pid {
        metadata.pid = Some(pid);
        let _ = write_pid_file(dir, pid);
    }
    if is_terminal_live_status(status) {
        metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
    }
    let _ = write_live_deployment_metadata(dir, &metadata);
}

fn is_terminal_live_status(status: &str) -> bool {
    matches!(status, "stopped" | "runtime-error" | "liquidated")
}

fn effective_live_status(metadata: &LiveDeploymentMetadata) -> String {
    if matches!(metadata.status.as_str(), "running" | "launching") {
        if running_live_pid(metadata).is_some() {
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
    let should_clear_pid = effective_status != "running" && metadata.pid.is_some();
    let should_update_status = metadata.status != effective_status;
    if should_clear_pid || should_update_status {
        metadata.status = effective_status;
        if metadata.status != "running" {
            metadata.pid = None;
            let _ = std::fs::remove_file(dir.join("pid"));
            if metadata.stopped.is_none() {
                metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
            }
        }
        metadata.updated_at = chrono::Utc::now().to_rfc3339();
        let _ = write_live_deployment_metadata(dir, &metadata);
    }
    metadata
}

fn running_live_pid(metadata: &LiveDeploymentMetadata) -> Option<u32> {
    if !matches!(metadata.status.as_str(), "running" | "launching") {
        return None;
    }
    let pid = metadata.pid?;
    if process_matches_live_deployment(pid, &metadata.deployment_dir) {
        Some(pid)
    } else {
        None
    }
}

fn process_matches_live_deployment(pid: u32, deployment_dir: &Path) -> bool {
    if !process_is_alive(pid) {
        return false;
    }
    let Ok(output) = ProcessCommand::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let deployment_dir = deployment_dir.to_string_lossy();
    command.contains("--live-deploy-dir") && command.contains(deployment_dir.as_ref())
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        let error = std::io::Error::last_os_error();
        error.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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

fn print_json_file(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => print_json_value(&value),
        Err(_) => {
            print!("{text}");
            Ok(())
        }
    }
}

fn print_json_value(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
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
    fn test_live_child_args_include_foreground_and_deploy_dir() {
        let args = RunArgs {
            strategy: PathBuf::from("/tmp/strategy/main.py"),
            runtime: RuntimeArgs {
                data_sidecar: Some("tcp://127.0.0.1:50051".to_string()),
                data_sidecar_token: None,
                data_provider_historical: Some("massive".to_string()),
                live_data_feed: Some("tradier".to_string()),
                brokerage: Some("hyperliquid".to_string()),
                brokerage_account: Some("account-1234".to_string()),
                start_date: None,
                end_date: None,
                parameters: vec!["foo=bar".to_string()],
                artifact_store: None,
                artifact_s3: None,
                verbose: true,
            },
            live_limits: LiveLimitArgs {
                live_max_slices: Some(2),
                live_max_runtime_seconds: Some(10),
            },
        };
        let child_args = live_child_args(&args, Path::new("/tmp/strategy/live/deploy"));

        assert_eq!(child_args[0], "live");
        assert!(child_args.contains(&"--foreground".to_string()));
        assert!(child_args.contains(&"--live-deploy-dir".to_string()));
        assert!(!child_args.contains(&"--live-trading".to_string()));
        assert!(child_args.contains(&"/tmp/strategy/live/deploy".to_string()));
        assert!(child_args.contains(&"--parameter".to_string()));
        assert!(child_args.contains(&"foo=bar".to_string()));
        assert!(child_args.contains(&"--data-sidecar".to_string()));
        assert!(child_args.contains(&"--data-provider-historical".to_string()));
        assert!(child_args.contains(&"massive".to_string()));
        assert!(child_args.contains(&"--live-data-feed".to_string()));
        assert!(child_args.contains(&"--brokerage-account".to_string()));
        assert!(child_args.contains(&"account-1234".to_string()));
        assert!(!child_args.contains(&"--data-sidecar-token".to_string()));
        assert!(child_args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn test_rewrite_live_command_strategy_uses_snapshot_path() {
        let mut command = vec![
            "/usr/local/bin/rlean".to_string(),
            "live".to_string(),
            "--foreground".to_string(),
            "--live-deploy-dir".to_string(),
            "/tmp/deploy".to_string(),
            "/tmp/source/main.py".to_string(),
            "--parameter".to_string(),
            "mode=review".to_string(),
            "--verbose".to_string(),
        ];

        rewrite_live_command_strategy(&mut command, Path::new("/tmp/deploy/code/main.py"));

        assert_eq!(command[5], "/tmp/deploy/code/main.py");
        assert_eq!(command[7], "mode=review");
    }

    #[test]
    fn test_mark_live_deployment_running_preserves_terminal_child_status() {
        let root = std::env::temp_dir().join(format!(
            "rlean-live-terminal-status-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let stale = LiveDeploymentMetadata {
            deploy_id: "deploy-1".to_string(),
            strategy: PathBuf::from("/tmp/source/main.py"),
            strategy_name: "source".to_string(),
            deployment_dir: root.clone(),
            pid: None,
            status: "paused".to_string(),
            launched: "2026-06-22T00:00:00Z".to_string(),
            stopped: Some("2026-06-22T00:01:00Z".to_string()),
            updated_at: "2026-06-22T00:01:00Z".to_string(),
            brokerage: Some("paper".to_string()),
            brokerage_account: None,
            paper_trading: true,
            command: vec![
                "/usr/local/bin/rlean".to_string(),
                "live".to_string(),
                "--foreground".to_string(),
                "--live-deploy-dir".to_string(),
                root.to_string_lossy().to_string(),
                "/tmp/source/main.py".to_string(),
            ],
            error: None,
            exit_code: None,
        };
        let mut terminal = stale.clone();
        terminal.pid = Some(42);
        terminal.status = "runtime-error".to_string();
        terminal.stopped = Some("2026-06-22T00:02:00Z".to_string());
        terminal.updated_at = "2026-06-22T00:02:00Z".to_string();
        terminal.error = Some("boom".to_string());
        terminal.exit_code = Some(1);
        write_live_deployment_metadata(&root, &terminal).unwrap();

        let result = mark_live_deployment_running(&root, &stale, 42).unwrap();
        let persisted = read_live_deployment_metadata(&root).unwrap();

        assert_eq!(result.status, "runtime-error");
        assert_eq!(persisted.status, "runtime-error");
        assert_eq!(persisted.pid, Some(42));
        assert_eq!(persisted.error.as_deref(), Some("boom"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
