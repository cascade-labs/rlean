//! `rlean cloud deploy` and the `status` / `logs` / `portfolio` monitors.
//!
//! Deploy snapshots a local strategy working tree to a node with rsync, then
//! launches `rlean live --pull` on the node so the host CLI pulls the latest
//! engine image and places a Docker container. The remote deploy directory +
//! `deployment.json` remain the durable strategy state; Docker restart policy
//! owns process keep-alive.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use super::deployments::{CloudDeployment, CloudDeploymentRegistry};
use super::registry::{Node, NodeRegistry};
use super::remote::{rsync_to, RemoteExec};

/// Subdirs excluded from the working-tree rsync (heavy / irrelevant / secret).
const RSYNC_EXCLUDES: &[&str] = &["backtests/", "live/", "__pycache__/", ".git/", "*.bak"];

/// Options for a deploy launch.
#[derive(Debug, Clone)]
pub(crate) struct DeployOptions {
    pub brokerage: String,
    pub live_data_feed: String,
}

impl DeployOptions {
    pub(crate) fn from_cli(brokerage: String, live_data_feed: String) -> Self {
        Self {
            brokerage,
            live_data_feed,
        }
    }
}

/// Derive the strategy name from a local strategy directory or main.py path.
///
/// Mirrors `runtime::strategy_name_from_path`: a `main.py` uses the parent
/// directory name; any other file uses its stem; a directory uses its own name.
pub(crate) fn strategy_name_from_dir(strategy_dir: &Path) -> String {
    strategy_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("strategy")
        .to_string()
}

/// The absolute node paths for a strategy (the deploy dir is created by the
/// node's own `rlean live` launcher under `<strategy_dir>/live/<deploy-id>`).
pub(crate) struct NodePaths {
    pub strategy_dir: String,
    pub main_py: String,
}

pub(crate) fn node_paths(node_home: &str, strategy_name: &str) -> NodePaths {
    let node_home = node_home.trim_end_matches('/');
    let strategy_dir = format!("{node_home}/rlean-cloud/workspace/{strategy_name}");
    NodePaths {
        main_py: format!("{strategy_dir}/main.py"),
        strategy_dir,
    }
}

/// Build the shell command run on the node to launch the deployment.
///
/// This invokes the node's own `rlean live --pull` launcher (NOT `--foreground`):
/// the host CLI pulls the latest engine image, reserves the deploy dir, snapshots
/// the code, writes `deployment.json`, and starts a Docker container with
/// `restart: unless-stopped`. The launcher exits after confirming startup,
/// printing `Live deployment started: deploy_id=<id> container=<id> ...`, which
/// [`parse_launch_line`] consumes.
///
/// Extracted as a pure string builder so the exact command is unit-testable.
pub(crate) fn remote_launch_command(paths: &NodePaths, opts: &DeployOptions) -> String {
    format!(
        "cd {strategy} && rlean live --pull {main} \
         --live-data-feed {feed} \
         --brokerage {brok}",
        strategy = paths.strategy_dir,
        main = paths.main_py,
        feed = opts.live_data_feed,
        brok = opts.brokerage,
    )
}

/// Parse the launcher's `Live deployment started: deploy_id=<id> ...` stdout
/// line into `(deploy_id, optional numeric id)`. Prefers `container=` and
/// falls back to legacy `pid=` for older launchers.
pub(crate) fn parse_launch_line(stdout: &str) -> Option<(String, u32)> {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("Live deployment started:"))?;
    let mut deploy_id = None;
    let mut numeric_id = None;
    for token in line.split_whitespace() {
        if let Some(v) = token.strip_prefix("deploy_id=") {
            deploy_id = Some(v.to_string());
        } else if let Some(v) = token.strip_prefix("pid=") {
            numeric_id = v.parse::<u32>().ok();
        } else if let Some(v) = token.strip_prefix("container=") {
            // Container ids are hex; keep a stable u32 fingerprint for the
            // cloud registry which still stores an optional pid field.
            let hex: String = v.chars().filter(|c| c.is_ascii_hexdigit()).take(8).collect();
            numeric_id = u32::from_str_radix(&hex, 16).ok().or(Some(0));
        }
    }
    Some((deploy_id?, numeric_id.unwrap_or(0)))
}

/// Build the argv for a `rlean live <sub> <deploy_id> ...` command run on the
/// node with cwd = the strategy dir (so the deploy resolves under `<cwd>/live/`).
///
/// The command is `sh -c 'cd <strategy_dir> && rlean live <sub...>'` so the cwd
/// is honored over ssh.
pub(crate) fn live_control_command(strategy_dir: &str, live_args: &[&str]) -> String {
    let joined = live_args.join(" ");
    format!("cd {strategy_dir} && rlean live {joined}")
}

// ── Deploy ───────────────────────────────────────────────────────────────────

/// Resolve the local strategy directory + its main.py, erroring if either is
/// missing. Accepts either a directory or a path to `main.py`.
pub(crate) fn resolve_local_strategy(input: &Path) -> Result<(PathBuf, String)> {
    let dir = if input.is_file() {
        input
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("strategy file has no parent directory"))?
    } else {
        input.to_path_buf()
    };
    if !dir.is_dir() {
        bail!("strategy directory does not exist: {}", dir.display());
    }
    let main = dir.join("main.py");
    if !main.is_file() {
        bail!("strategy directory has no main.py: {}", dir.display());
    }
    let name = strategy_name_from_dir(&dir);
    Ok((dir, name))
}

/// Result of a deploy launch, for printing/testing.
pub(crate) struct DeploySummary {
    pub deploy_id: String,
    pub deploy_dir: String,
    pub pid: Option<u32>,
    pub startup_status: String,
}

/// The inputs to a deploy launch, grouped so `cmd_deploy` stays a two-arg call.
pub(crate) struct DeployRequest<'a> {
    pub node: &'a Node,
    pub node_home: &'a str,
    pub local_strategy_dir: &'a Path,
    pub strategy_name: &'a str,
    pub opts: &'a DeployOptions,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// Snapshot + launch a strategy on a node, recording it in the cloud registry.
pub(crate) fn cmd_deploy(
    exec: &dyn RemoteExec,
    req: &DeployRequest<'_>,
    reg: &mut CloudDeploymentRegistry,
) -> Result<DeploySummary> {
    let DeployRequest {
        node,
        node_home,
        local_strategy_dir,
        strategy_name,
        opts,
        now,
    } = *req;
    let ssh = &node.ssh_dest;
    let paths = node_paths(node_home, strategy_name);

    // 1. rsync the working tree to the node.
    rsync_to(ssh, local_strategy_dir, &paths.strategy_dir, RSYNC_EXCLUDES)?;

    // 2. Launch via the node's own `rlean live --pull` launcher; it pulls the
    // latest engine image, reserves the deploy dir, writes deployment.json,
    // and places a Docker container.
    // The compound command is passed as a SINGLE argv element: OpenSSH joins
    // remote argv with spaces (no quoting), so a ["sh","-c",cmd] triple would
    // arrive as `sh -c cd ...` and `sh -c` would receive only `cd` — the rest
    // of the chain would run in the wrong shell and cwd. One element arrives
    // intact and the remote login shell evaluates the whole line.
    let launch = remote_launch_command(&paths, opts);
    let out = exec.run(ssh, &[launch.as_str()])?;
    if out.status != 0 {
        bail!(
            "failed to launch deployment on '{}' (status {}):\n{}{}",
            node.name,
            out.status,
            out.stdout.trim(),
            out.stderr.trim()
        );
    }
    let (id, pid) = parse_launch_line(&out.stdout).ok_or_else(|| {
        anyhow::anyhow!(
            "node launcher did not report a deployment (stdout: {})",
            out.stdout.trim()
        )
    })?;
    let deploy_dir = format!("{}/live/{}", paths.strategy_dir, id);

    // 3. Record locally.
    reg.record(CloudDeployment {
        node: node.name.clone(),
        strategy_name: strategy_name.to_string(),
        deploy_id: id.clone(),
        strategy_dir: paths.strategy_dir.clone(),
        deploy_dir: deploy_dir.clone(),
        launched_at: now.to_rfc3339(),
        pid: Some(pid),
    });

    // 4. Poll node status until running or an error surfaces.
    let startup_status = poll_startup(exec, ssh, &paths.strategy_dir, &id)?;

    Ok(DeploySummary {
        deploy_id: id,
        deploy_dir,
        pid: Some(pid),
        startup_status,
    })
}

/// Poll `rlean live status <id>` on the node a few times, returning the last
/// observed effective status. Stops early on `running` or a terminal status.
fn poll_startup(
    exec: &dyn RemoteExec,
    ssh: &str,
    strategy_dir: &str,
    deploy_id: &str,
) -> Result<String> {
    let cmd = live_control_command(strategy_dir, &["status", deploy_id]);
    let mut last = "unknown".to_string();
    for attempt in 0..6 {
        let out = exec.run(ssh, &[cmd.as_str()])?;
        if out.status == 0 {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&out.stdout) {
                if let Some(status) = value.get("status").and_then(|s| s.as_str()) {
                    last = status.to_string();
                    if status == "running"
                        || matches!(status, "stopped" | "runtime-error" | "liquidated")
                    {
                        return Ok(last);
                    }
                }
            }
        }
        // The status file may not exist yet on the first probes; keep polling.
        if attempt < 5 {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
    }
    Ok(last)
}

// ── status / logs / portfolio (rendered over ssh) ───────────────────────────────

/// Run `rlean live status <id>` on the node and return its stdout.
pub(crate) fn remote_status(
    exec: &dyn RemoteExec,
    ssh: &str,
    strategy_dir: &str,
    deploy_id: &str,
) -> Result<String> {
    run_live_control(exec, ssh, strategy_dir, &["status", deploy_id])
}

/// Run `rlean live logs <id> --lines N` on the node and return its stdout.
pub(crate) fn remote_logs(
    exec: &dyn RemoteExec,
    ssh: &str,
    strategy_dir: &str,
    deploy_id: &str,
    lines: usize,
) -> Result<String> {
    let lines = lines.to_string();
    run_live_control(
        exec,
        ssh,
        strategy_dir,
        &["logs", deploy_id, "--lines", &lines],
    )
}

/// Run `rlean live portfolio <id>` on the node and return its stdout.
pub(crate) fn remote_portfolio(
    exec: &dyn RemoteExec,
    ssh: &str,
    strategy_dir: &str,
    deploy_id: &str,
) -> Result<String> {
    run_live_control(exec, ssh, strategy_dir, &["portfolio", deploy_id])
}

/// Run `rlean live list --status running` on the node and return its stdout.
pub(crate) fn remote_list_running(exec: &dyn RemoteExec, ssh: &str) -> Result<String> {
    let out = exec.run(ssh, &["rlean", "live", "list", "--status", "running"])?;
    if out.status != 0 {
        bail!(
            "`rlean live list` failed on node (status {}):\n{}",
            out.status,
            out.stderr.trim()
        );
    }
    Ok(out.stdout)
}

fn run_live_control(
    exec: &dyn RemoteExec,
    ssh: &str,
    strategy_dir: &str,
    live_args: &[&str],
) -> Result<String> {
    let cmd = live_control_command(strategy_dir, live_args);
    let out = exec.run(ssh, &[cmd.as_str()])?;
    if out.status != 0 {
        bail!(
            "`rlean live {}` failed on node (status {}):\n{}",
            live_args.join(" "),
            out.status,
            out.stderr.trim()
        );
    }
    Ok(out.stdout)
}

/// Resolve `(strategy_dir, deploy_id)` for a status/logs/portfolio command from
/// the cloud-deployments registry (given an optional explicit deploy id).
pub(crate) fn resolve_target<'a>(
    reg: &'a CloudDeploymentRegistry,
    node: &str,
    deploy_id: Option<&str>,
) -> Result<&'a CloudDeployment> {
    match deploy_id {
        Some(id) => reg
            .find(node, id)
            .ok_or_else(|| anyhow::anyhow!("no recorded deployment '{id}' on node '{node}'")),
        None => reg.resolve_latest(node),
    }
}

// ── Aggregated status across nodes ──────────────────────────────────────────────

/// A row for the aggregated status table.
pub(crate) struct AggregatedRow {
    pub node: String,
    pub deploy_id: String,
    pub status: String,
    pub pid: String,
}

/// Query each node with a recorded deployment and collect the effective status
/// of every recorded deploy into table rows.
pub(crate) fn aggregate_status(
    exec: &dyn RemoteExec,
    node_reg: &NodeRegistry,
    deploy_reg: &CloudDeploymentRegistry,
) -> Vec<AggregatedRow> {
    let mut rows = Vec::new();
    for d in &deploy_reg.deployments {
        let Some(node) = node_reg.get(&d.node) else {
            rows.push(AggregatedRow {
                node: d.node.clone(),
                deploy_id: d.deploy_id.clone(),
                status: "unknown-node".to_string(),
                pid: "-".to_string(),
            });
            continue;
        };
        let (status, pid) = match remote_status(exec, &node.ssh_dest, &d.strategy_dir, &d.deploy_id)
        {
            Ok(stdout) => match serde_json::from_str::<serde_json::Value>(&stdout) {
                Ok(v) => (
                    v.get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    v.get("pid")
                        .and_then(|p| p.as_u64())
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                ),
                Err(_) => ("unparseable".to_string(), "-".to_string()),
            },
            Err(_) => ("unreachable".to_string(), "-".to_string()),
        };
        rows.push(AggregatedRow {
            node: d.node.clone(),
            deploy_id: d.deploy_id.clone(),
            status,
            pid,
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::remote::RemoteOutput;
    use chrono::{TimeZone, Utc};
    use std::collections::HashMap;

    struct FakeExec {
        responses: HashMap<(String, String), RemoteOutput>,
    }
    impl FakeExec {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
            }
        }
        fn set(&mut self, ssh: &str, argv: &[&str], out: RemoteOutput) {
            self.responses
                .insert((ssh.to_string(), argv.join("\u{1}")), out);
        }
    }
    impl RemoteExec for FakeExec {
        fn run(&self, ssh: &str, argv: &[&str]) -> Result<RemoteOutput> {
            let key = (ssh.to_string(), argv.join("\u{1}"));
            let out = self
                .responses
                .get(&key)
                .unwrap_or_else(|| panic!("no fake response for {key:?}"));
            Ok(RemoteOutput {
                status: out.status,
                stdout: out.stdout.clone(),
                stderr: out.stderr.clone(),
            })
        }
    }
    fn ok(stdout: &str) -> RemoteOutput {
        RemoteOutput {
            status: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn parse_launch_line_extracts_id_and_pid() {
        let stdout = "Python baseline ok\nLive deployment started: deploy_id=2026-07-10_143005_uw_control pid=4242 dir=/x log=/x/live.log\n";
        let (id, pid) = parse_launch_line(stdout).unwrap();
        assert_eq!(id, "2026-07-10_143005_uw_control");
        assert_eq!(pid, 4242);
    }

    #[test]
    fn parse_launch_line_extracts_container_id() {
        let stdout = "Live deployment started: deploy_id=d1 container=a1b2c3d4e5f67890 dir=/x\n";
        let (id, fingerprint) = parse_launch_line(stdout).unwrap();
        assert_eq!(id, "d1");
        assert_eq!(fingerprint, 0xa1b2c3d4);
    }

    #[test]
    fn parse_launch_line_rejects_missing_marker() {
        assert!(parse_launch_line("engine crashed\n").is_none());
        assert!(parse_launch_line("Live deployment started: dir=/x\n").is_none());
    }

    #[test]
    fn node_paths_are_absolute_and_nested() {
        let p = node_paths("/home/opc/", "uw_control");
        assert_eq!(p.strategy_dir, "/home/opc/rlean-cloud/workspace/uw_control");
        assert_eq!(
            p.main_py,
            "/home/opc/rlean-cloud/workspace/uw_control/main.py"
        );
    }

    #[test]
    fn launch_command_uses_native_launcher() {
        let p = node_paths("/home/opc", "uw_control");
        let opts = DeployOptions {
            brokerage: "tradier".to_string(),
            live_data_feed: "tradier".to_string(),
        };
        let cmd = remote_launch_command(&p, &opts);
        // The node's own launcher pulls the image, owns detachment via Docker,
        // and writes deployment.json — the command must NOT hand-roll --foreground.
        assert!(cmd.starts_with(
            "cd /home/opc/rlean-cloud/workspace/uw_control && rlean live --pull "
        ));
        assert!(!cmd.contains("--foreground"));
        assert!(!cmd.contains("--live-deploy-dir"));
        assert!(cmd.contains(" /home/opc/rlean-cloud/workspace/uw_control/main.py"));
        assert!(cmd.contains("--brokerage tradier"));
        assert!(cmd.contains("--live-data-feed tradier"));
        assert!(!cmd.contains("--data-provider"));
        assert!(!cmd.contains("--data "));
    }

    #[test]
    fn live_control_command_sets_cwd() {
        let cmd = live_control_command("/home/opc/ws/uw", &["logs", "d1", "--lines", "40"]);
        assert_eq!(cmd, "cd /home/opc/ws/uw && rlean live logs d1 --lines 40");
    }

    #[test]
    fn deploy_options_apply_uw_defaults() {
        let opts = DeployOptions::from_cli("tradier".to_string(), "tradier".to_string());
        assert_eq!(opts.brokerage, "tradier");
        assert_eq!(opts.live_data_feed, "tradier");
    }

    #[test]
    fn cmd_deploy_records_and_reports_running() {
        let node = Node {
            name: "n1".to_string(),
            ssh_dest: "opc@n1".to_string(),
            os: "linux".to_string(),
            arch: "aarch64".to_string(),
            platform: "aarch64-unknown-linux-gnu".to_string(),
            uname: "Linux".to_string(),
            roles: vec!["live".to_string()],
            added_at: "2026-07-10T00:00:00+00:00".to_string(),
            last_seen: "2026-07-10T00:00:00+00:00".to_string(),
        };
        let now = Utc.with_ymd_and_hms(2026, 7, 10, 14, 30, 5).unwrap();
        let id = "2026-07-10_143005_uw_control";
        let opts = DeployOptions::from_cli("tradier".to_string(), "tradier".to_string());
        let paths = node_paths("/home/opc", "uw_control");

        let mut fake = FakeExec::new();
        let launch = remote_launch_command(&paths, &opts);
        fake.set(
            "opc@n1",
            &[&launch],
            ok(&format!(
                "Live deployment started: deploy_id={id} pid=4242 dir=/x log=/x/live.log\n"
            )),
        );
        let status_cmd = live_control_command(&paths.strategy_dir, &["status", id]);
        fake.set(
            "opc@n1",
            &[&status_cmd],
            ok(r#"{"status":"running","pid":4242}"#),
        );

        // rsync shells out to the real rsync, so exercise the launch + poll
        // path directly: run the launch command, parse the launcher line, and
        // poll status exactly as cmd_deploy does after its rsync step.
        let out = fake.run("opc@n1", &[&launch]).unwrap();
        let (parsed_id, pid) = parse_launch_line(&out.stdout).unwrap();
        assert_eq!(parsed_id, id);
        assert_eq!(pid, 4242);
        let status = poll_startup(&fake, "opc@n1", &paths.strategy_dir, id).unwrap();
        assert_eq!(status, "running");

        // And the registry record shape is what status/logs will consume.
        let mut reg = CloudDeploymentRegistry::default();
        reg.record(CloudDeployment {
            node: node.name.clone(),
            strategy_name: "uw_control".to_string(),
            deploy_id: id.to_string(),
            strategy_dir: paths.strategy_dir.clone(),
            deploy_dir: format!("{}/live/{id}", paths.strategy_dir),
            launched_at: now.to_rfc3339(),
            pid: Some(4242),
        });
        let resolved = resolve_target(&reg, "n1", None).unwrap();
        assert_eq!(resolved.deploy_id, id);
        assert_eq!(resolved.strategy_dir, paths.strategy_dir);
    }

    #[test]
    fn remote_status_logs_portfolio_build_expected_commands() {
        let mut fake = FakeExec::new();
        let dir = "/home/opc/rlean-cloud/workspace/uw";
        fake.set(
            "n1",
            &[&live_control_command(dir, &["status", "d1"])],
            ok(r#"{"status":"running"}"#),
        );
        fake.set(
            "n1",
            &[&live_control_command(dir, &["logs", "d1", "--lines", "10"])],
            ok("log line\n"),
        );
        fake.set(
            "n1",
            &[&live_control_command(dir, &["portfolio", "d1"])],
            ok("{}"),
        );

        assert!(remote_status(&fake, "n1", dir, "d1")
            .unwrap()
            .contains("running"));
        assert!(remote_logs(&fake, "n1", dir, "d1", 10)
            .unwrap()
            .contains("log line"));
        assert_eq!(remote_portfolio(&fake, "n1", dir, "d1").unwrap(), "{}");
    }

    #[test]
    fn aggregate_status_collects_per_node_rows() {
        let mut node_reg = NodeRegistry::default();
        node_reg
            .add(Node {
                name: "n1".to_string(),
                ssh_dest: "opc@n1".to_string(),
                os: "linux".to_string(),
                arch: "aarch64".to_string(),
                platform: "aarch64-unknown-linux-gnu".to_string(),
                uname: "Linux".to_string(),
                roles: vec!["live".to_string()],
                added_at: "2026-07-10T00:00:00+00:00".to_string(),
                last_seen: "2026-07-10T00:00:00+00:00".to_string(),
            })
            .unwrap();

        let mut deploy_reg = CloudDeploymentRegistry::default();
        deploy_reg.record(CloudDeployment {
            node: "n1".to_string(),
            strategy_name: "uw".to_string(),
            deploy_id: "d1".to_string(),
            strategy_dir: "/home/opc/ws/uw".to_string(),
            deploy_dir: "/home/opc/ws/uw/live/d1".to_string(),
            launched_at: "2026-07-10T00:00:00+00:00".to_string(),
            pid: Some(7),
        });

        let mut fake = FakeExec::new();
        fake.set(
            "opc@n1",
            &[&live_control_command("/home/opc/ws/uw", &["status", "d1"])],
            ok(r#"{"status":"running","pid":7}"#),
        );

        let rows = aggregate_status(&fake, &node_reg, &deploy_reg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node, "n1");
        assert_eq!(rows[0].status, "running");
        assert_eq!(rows[0].pid, "7");
    }

    #[test]
    fn resolve_target_requires_deploy_id_when_ambiguous() {
        let mut reg = CloudDeploymentRegistry::default();
        reg.record(CloudDeployment {
            node: "n1".to_string(),
            strategy_name: "a".to_string(),
            deploy_id: "d1".to_string(),
            strategy_dir: "/x".to_string(),
            deploy_dir: "/x/live/d1".to_string(),
            launched_at: "2026-07-10T00:00:00+00:00".to_string(),
            pid: None,
        });
        reg.record(CloudDeployment {
            node: "n1".to_string(),
            strategy_name: "a".to_string(),
            deploy_id: "d2".to_string(),
            strategy_dir: "/x".to_string(),
            deploy_dir: "/x/live/d2".to_string(),
            launched_at: "2026-07-10T01:00:00+00:00".to_string(),
            pid: None,
        });
        assert!(resolve_target(&reg, "n1", None).is_err());
        assert_eq!(
            resolve_target(&reg, "n1", Some("d2")).unwrap().deploy_id,
            "d2"
        );
    }
}
