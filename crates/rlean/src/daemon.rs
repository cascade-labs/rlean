//! Persistent live-deployment supervisor shared by the `rlean` CLI and `rleand`.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

const SOCKET_NAME: &str = "rleand.sock";
const REGISTRY_NAME: &str = "rleand-deployments.json";
const MAX_RESTART_DELAY: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeploymentSpec {
    pub deploy_id: String,
    pub deployment_dir: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub working_dir: PathBuf,
    pub log_path: PathBuf,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default = "running")]
    pub desired_state: String,
    /// True when this request resumes an existing deployment rather than
    /// creating its first process.
    #[serde(default)]
    pub restart: bool,
}

fn running() -> String {
    "running".to_owned()
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Ping,
    Start(DeploymentSpec),
    Stop {
        deploy_id: String,
        timeout_seconds: u64,
    },
    Remove {
        deploy_id: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
    pub pid: Option<u32>,
}

pub fn rlean_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("RLEAN_HOME") {
        return Ok(PathBuf::from(path));
    }
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join(".rlean"))
        .context("HOME is not set")
}

pub fn socket_path() -> Result<PathBuf> {
    Ok(rlean_home()?.join(SOCKET_NAME))
}

fn registry_path() -> Result<PathBuf> {
    Ok(rlean_home()?.join(REGISTRY_NAME))
}

pub fn request(request: &Request) -> Result<Response> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let path = socket_path()?;
        let mut stream = UnixStream::connect(&path).with_context(|| {
            format!(
                "rleand is not reachable at {}; run `rlean daemon install`",
                path.display()
            )
        })?;
        serde_json::to_writer(&mut stream, request)?;
        stream.write_all(b"\n")?;
        stream.flush()?;
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line)?;
        let response: Response = serde_json::from_str(&line).context("invalid rleand response")?;
        if !response.ok {
            bail!(response.message.clone());
        }
        Ok(response)
    }
    #[cfg(not(unix))]
    {
        let _ = request;
        bail!("rleand requires Unix-domain sockets")
    }
}

struct Managed {
    spec: DeploymentSpec,
    child: Option<Child>,
    failures: u32,
    restart_at: Option<Instant>,
    is_restart: bool,
}

impl Managed {
    fn new(spec: DeploymentSpec, is_restart: bool) -> Self {
        Self {
            spec,
            child: None,
            failures: 0,
            restart_at: None,
            is_restart,
        }
    }
}

pub fn run() -> Result<()> {
    #[cfg(not(unix))]
    bail!("rleand is currently supported on macOS and Linux");

    #[cfg(unix)]
    {
        use std::os::unix::net::UnixListener;

        let home = rlean_home()?;
        std::fs::create_dir_all(&home)?;
        let socket = socket_path()?;
        if socket.exists() {
            std::fs::remove_file(&socket)?;
        }
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("failed to bind {}", socket.display()))?;
        listener.set_nonblocking(true)?;
        set_owner_only(&socket)?;

        let mut saved = load_registry()?;
        adopt_registered_running_deployments(&mut saved)?;
        let mut deployments: BTreeMap<String, Managed> = saved
            .into_iter()
            .map(|spec| (spec.deploy_id.clone(), Managed::new(spec, true)))
            .collect();
        // A daemon restart deliberately rebuilds every desired live process.
        // Kill a still-present prior child before launching its replacement.
        for managed in deployments.values_mut() {
            if managed.spec.desired_state == "running" {
                terminate_recorded_child(&managed.spec.deployment_dir);
                managed.restart_at = Some(Instant::now());
            }
        }

        tracing::info!(socket = %socket.display(), deployments = deployments.len(), "rleand ready");
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = handle_stream(stream, &mut deployments) {
                        tracing::warn!("rleand control request failed: {error}");
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            supervise(&mut deployments)?;
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(unix)]
fn handle_stream(
    mut stream: std::os::unix::net::UnixStream,
    deployments: &mut BTreeMap<String, Managed>,
) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(Request::Ping) => Response {
            ok: true,
            message: "rleand is running".to_owned(),
            pid: Some(std::process::id()),
        },
        Ok(Request::Start(mut spec)) => {
            spec.desired_state = "running".to_owned();
            let id = spec.deploy_id.clone();
            if let Some(existing) = deployments.get_mut(&id) {
                stop_child(existing, Duration::from_secs(30));
            }
            let is_restart = spec.restart;
            let mut managed = Managed::new(spec, is_restart);
            let result = spawn_child(&mut managed);
            let pid = managed.child.as_ref().map(Child::id);
            deployments.insert(id, managed);
            save_registry(deployments)?;
            match result {
                Ok(()) => Response {
                    ok: true,
                    message: "deployment started".to_owned(),
                    pid,
                },
                Err(error) => Response {
                    ok: false,
                    message: error.to_string(),
                    pid: None,
                },
            }
        }
        Ok(Request::Stop {
            deploy_id,
            timeout_seconds,
        }) => match deployments.get_mut(&deploy_id) {
            Some(managed) => {
                managed.spec.desired_state = "paused".to_owned();
                managed.restart_at = None;
                stop_child(managed, Duration::from_secs(timeout_seconds));
                save_registry(deployments)?;
                Response {
                    ok: true,
                    message: "deployment stopped".to_owned(),
                    pid: None,
                }
            }
            None => Response {
                ok: false,
                message: format!("deployment is not managed by rleand: {deploy_id}"),
                pid: None,
            },
        },
        Ok(Request::Remove { deploy_id }) => {
            if let Some(mut managed) = deployments.remove(&deploy_id) {
                stop_child(&mut managed, Duration::from_secs(30));
            }
            save_registry(deployments)?;
            Response {
                ok: true,
                message: "deployment removed".to_owned(),
                pid: None,
            }
        }
        Err(error) => Response {
            ok: false,
            message: format!("invalid request: {error}"),
            pid: None,
        },
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    Ok(())
}

fn supervise(deployments: &mut BTreeMap<String, Managed>) -> Result<()> {
    let now = Instant::now();
    let mut changed = false;
    for managed in deployments.values_mut() {
        if let Some(child) = managed.child.as_mut() {
            if let Some(status) = child.try_wait()? {
                managed.child = None;
                managed.is_restart = true;
                if managed.spec.desired_state != "running" {
                    managed.restart_at = None;
                    continue;
                }
                if status.success() {
                    managed.spec.desired_state = "stopped".to_owned();
                    managed.restart_at = None;
                    changed = true;
                    continue;
                }
                if transient_sidecar_failure(&managed.spec.deployment_dir) {
                    managed.failures = managed.failures.saturating_add(1);
                    let delay = restart_delay(managed.failures);
                    managed.restart_at = Some(now + delay);
                    mark_restarting(&managed.spec.deployment_dir, delay);
                    tracing::warn!(
                        deployment = %managed.spec.deploy_id,
                        ?delay,
                        "live process lost its sidecar session; scheduling restart"
                    );
                    changed = true;
                } else {
                    managed.spec.desired_state = "runtime-error".to_owned();
                    managed.restart_at = None;
                    tracing::error!(deployment = %managed.spec.deploy_id, %status, "live process failed; not restarting non-sidecar error");
                    changed = true;
                }
            }
        }
        if managed.child.is_none()
            && managed.spec.desired_state == "running"
            && managed.restart_at.is_some_and(|deadline| now >= deadline)
        {
            match spawn_child(managed) {
                Ok(()) => {
                    managed.restart_at = None;
                    changed = true;
                }
                Err(error) => {
                    if error.to_string().starts_with("refusing live restart") {
                        managed.spec.desired_state = "runtime-error".to_owned();
                        managed.restart_at = None;
                        mark_daemon_runtime_error(&managed.spec.deployment_dir, &error.to_string());
                        tracing::error!(deployment = %managed.spec.deploy_id, "{error}");
                        changed = true;
                        continue;
                    }
                    managed.failures = managed.failures.saturating_add(1);
                    let delay = restart_delay(managed.failures);
                    managed.restart_at = Some(now + delay);
                    tracing::warn!(deployment = %managed.spec.deploy_id, ?delay, "restart failed: {error}");
                }
            }
        }
    }
    if changed {
        save_registry(deployments)?;
    }
    Ok(())
}

fn spawn_child(managed: &mut Managed) -> Result<()> {
    if managed.is_restart {
        validate_restart_checkpoint(&managed.spec.deployment_dir)?;
    }
    std::fs::create_dir_all(&managed.spec.deployment_dir)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&managed.spec.log_path)?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(&managed.spec.program);
    command
        .args(&managed.spec.args)
        .current_dir(&managed.spec.working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .envs(&managed.spec.environment);
    let child = command
        .spawn()
        .with_context(|| format!("failed to start live deployment {}", managed.spec.deploy_id))?;
    tracing::info!(deployment = %managed.spec.deploy_id, pid = child.id(), "live process started");
    managed.child = Some(child);
    managed.is_restart = true;
    Ok(())
}

/// Once a deployment has written brokerage/account state, restarting without
/// its matching insight state could make startup convergence classify every
/// holding as unmanaged. A process that failed before its first account
/// snapshot is safe to retry because it never reached brokerage convergence.
fn validate_restart_checkpoint(deployment_dir: &Path) -> Result<()> {
    if !deployment_dir.join("portfolio.json").exists() {
        return Ok(());
    }
    let path = deployment_dir.join("insights.json");
    let bytes = std::fs::read(&path).with_context(|| {
        format!(
            "refusing live restart: account state exists but insight checkpoint is missing ({})",
            path.display()
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "refusing live restart: insight checkpoint is corrupt ({})",
            path.display()
        )
    })?;
    if value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        != Some(1)
        || !value.get("state").is_some_and(serde_json::Value::is_object)
    {
        bail!(
            "refusing live restart: insight checkpoint has an unsupported schema ({})",
            path.display()
        );
    }
    Ok(())
}

fn stop_child(managed: &mut Managed, timeout: Duration) {
    if let Some(mut child) = managed.child.take() {
        terminate_pid(child.id());
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if child.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn restart_delay(failures: u32) -> Duration {
    Duration::from_secs(
        (1u64 << failures.saturating_sub(1).min(6)).min(MAX_RESTART_DELAY.as_secs()),
    )
}

fn transient_sidecar_failure(deployment_dir: &Path) -> bool {
    let Ok(value) = read_deployment_json(deployment_dir) else {
        return false;
    };
    let error = value
        .get("error")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    [
        "sidecar",
        "flight exchange",
        "failed to open live data feed",
        "failed to open sidecar brokerage",
        "transport error",
        "connection reset",
        "broken pipe",
        "connection refused",
        "status: unavailable",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn mark_restarting(deployment_dir: &Path, delay: Duration) {
    let Ok(mut value) = read_deployment_json(deployment_dir) else {
        return;
    };
    value["status"] = serde_json::Value::String("restarting".to_owned());
    value["pid"] = serde_json::Value::Null;
    value["updated_at"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
    value["error"] = serde_json::Value::String(format!(
        "sidecar unavailable; rleand retrying in {}s",
        delay.as_secs()
    ));
    let _ = write_json_atomic(&deployment_dir.join("deployment.json"), &value);
    let _ = std::fs::remove_file(deployment_dir.join("pid"));
}

fn mark_daemon_runtime_error(deployment_dir: &Path, error: &str) {
    let Ok(mut value) = read_deployment_json(deployment_dir) else {
        return;
    };
    value["status"] = serde_json::Value::String("runtime-error".to_owned());
    value["pid"] = serde_json::Value::Null;
    value["updated_at"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
    value["error"] = serde_json::Value::String(error.to_owned());
    let _ = write_json_atomic(&deployment_dir.join("deployment.json"), &value);
    let _ = std::fs::remove_file(deployment_dir.join("pid"));
}

fn read_deployment_json(dir: &Path) -> Result<serde_json::Value> {
    Ok(serde_json::from_slice(&std::fs::read(
        dir.join("deployment.json"),
    )?)?)
}

fn load_registry() -> Result<Vec<DeploymentSpec>> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&std::fs::read(&path)?)
        .with_context(|| format!("failed to parse {}", path.display()))
}

/// One-time migration from the pre-daemon deployment registry. A deployment
/// that was running when rleand is first installed becomes daemon-owned without
/// requiring the operator to pause and resume it manually.
fn adopt_registered_running_deployments(specs: &mut Vec<DeploymentSpec>) -> Result<()> {
    let path = rlean_home()?.join("live/registry.json");
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let dirs: Vec<PathBuf> = serde_json::from_slice::<Vec<String>>(&bytes)?
        .into_iter()
        .map(PathBuf::from)
        .collect();
    for dir in dirs {
        let Ok(value) = read_deployment_json(&dir) else {
            continue;
        };
        let status = value
            .get("status")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        if !matches!(status, "running" | "launching" | "restarting") {
            continue;
        }
        let Some(deploy_id) = value
            .get("deploy_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            continue;
        };
        if specs.iter().any(|spec| spec.deploy_id == deploy_id) {
            continue;
        }
        let command = value
            .get("command")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let Some((program, args)) = command.split_first() else {
            continue;
        };
        let working_dir = dir.join("code");
        specs.push(DeploymentSpec {
            deploy_id,
            deployment_dir: dir.clone(),
            program: PathBuf::from(program),
            args: args.to_vec(),
            working_dir,
            log_path: dir.join("live.log"),
            environment: BTreeMap::new(),
            desired_state: "running".to_owned(),
            restart: true,
        });
    }
    Ok(())
}

fn save_registry(deployments: &BTreeMap<String, Managed>) -> Result<()> {
    let specs = deployments
        .values()
        .map(|managed| managed.spec.clone())
        .collect::<Vec<_>>();
    let path = registry_path()?;
    write_json_atomic(&path, &serde_json::to_value(specs)?)?;
    set_owner_only(&path)
}

fn write_json_atomic(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

fn terminate_recorded_child(deployment_dir: &Path) {
    let Ok(value) = read_deployment_json(deployment_dir) else {
        return;
    };
    if let Some(pid) = value.get("pid").and_then(serde_json::Value::as_u64) {
        terminate_pid(pid as u32);
        thread::sleep(Duration::from_millis(250));
    }
}

fn terminate_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

fn set_owner_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn render_systemd_unit(program: &Path, user: bool) -> String {
    let wanted_by = if user {
        "default.target"
    } else {
        "multi-user.target"
    };
    format!(
        "[Unit]\nDescription=rlean live deployment supervisor\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={}\nRestart=always\nRestartSec=1\n\n[Install]\nWantedBy={}\n",
        program.display(), wanted_by
    )
}

pub fn render_launchd_plist(program: &Path, stdout: &Path, stderr: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict>\n<key>Label</key><string>com.cascadelabs.rleand</string>\n<key>ProgramArguments</key><array><string>{}</string></array>\n<key>RunAtLoad</key><true/>\n<key>KeepAlive</key><true/>\n<key>StandardOutPath</key><string>{}</string>\n<key>StandardErrorPath</key><string>{}</string>\n</dict></plist>\n",
        xml_escape(&program.display().to_string()),
        xml_escape(&stdout.display().to_string()),
        xml_escape(&stderr.display().to_string())
    )
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_errors_are_transient_but_python_errors_are_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("deployment.json"),
            r#"{"error":"Data error: Flight exchange closed"}"#,
        )
        .unwrap();
        assert!(transient_sidecar_failure(dir.path()));
        std::fs::write(
            dir.path().join("deployment.json"),
            r#"{"error":"failed to open live data feed 'robinhood'"}"#,
        )
        .unwrap();
        assert!(transient_sidecar_failure(dir.path()));
        std::fs::write(
            dir.path().join("deployment.json"),
            r#"{"error":"failed to open sidecar brokerage 'tradier'"}"#,
        )
        .unwrap();
        assert!(transient_sidecar_failure(dir.path()));
        std::fs::write(
            dir.path().join("deployment.json"),
            r#"{"error":"Python exception: division by zero"}"#,
        )
        .unwrap();
        assert!(!transient_sidecar_failure(dir.path()));
    }

    #[test]
    fn restart_backoff_is_bounded() {
        assert_eq!(restart_delay(1), Duration::from_secs(1));
        assert_eq!(restart_delay(4), Duration::from_secs(8));
        assert_eq!(restart_delay(20), Duration::from_secs(60));
    }

    #[test]
    fn service_definitions_restart_and_boot_start() {
        let unit = render_systemd_unit(Path::new("/usr/bin/rleand"), false);
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        let plist = render_launchd_plist(
            Path::new("/usr/local/bin/rleand"),
            Path::new("/tmp/out"),
            Path::new("/tmp/err"),
        );
        assert!(plist.contains("<key>KeepAlive</key><true/>"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn restart_with_account_state_requires_valid_insights() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("portfolio.json"), b"{}").unwrap();
        assert!(validate_restart_checkpoint(dir.path()).is_err());
        std::fs::write(dir.path().join("insights.json"), b"not-json").unwrap();
        assert!(validate_restart_checkpoint(dir.path()).is_err());
        std::fs::write(
            dir.path().join("insights.json"),
            br#"{"schema_version":1,"state":{"active":[],"closed":[]}}"#,
        )
        .unwrap();
        assert!(validate_restart_checkpoint(dir.path()).is_ok());
    }
}
