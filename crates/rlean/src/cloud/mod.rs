//! `rlean cloud` — manage a fleet of remote nodes reachable over SSH.
//!
//! Nodes are recorded in `~/.rlean/nodes.json`. Registration probes a node's
//! OS/arch over SSH and derives its Rust target triple so later commands know
//! which prebuilt binaries to ship to it.
//!
//! Usage:
//!   rlean cloud add-node <ssh-alias> [--name N] [--role live,backtest]
//!   rlean cloud list [--offline]
//!   rlean cloud remove <name>
//!   rlean cloud exec <name> -- <command...>
//!   rlean cloud install <name> [--release-tag T]
//!   rlean cloud deploy <name> <strategy-dir> [--brokerage B]
//!   rlean cloud status [<name>] [--deploy-id ID]
//!   rlean cloud logs <name> [--deploy-id ID] [--lines N]
//!   rlean cloud portfolio <name> [--deploy-id ID]

mod deploy;
mod deployments;
mod install;
mod nodeconfig;
mod registry;
mod remote;

use std::path::PathBuf;

use anyhow::{bail, Result};
use chrono::Utc;

use deploy::{
    aggregate_status, cmd_deploy, remote_logs, remote_portfolio, remote_status, resolve_target,
    DeployOptions, DeployRequest,
};
use deployments::CloudDeploymentRegistry;
use install::{cmd_install, probe_node_home, DEFAULT_RELEASE_TAG};
use registry::{platform_triple, Node, NodeRegistry};
use remote::{RemoteExec, SshExec};

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct CloudArgs {
    #[command(subcommand)]
    pub command: CloudCommand,
}

#[derive(clap::Subcommand)]
pub enum CloudCommand {
    /// Register a node reachable over SSH (probes its OS, arch, and platform)
    AddNode {
        /// SSH destination or `~/.ssh/config` alias
        ssh_alias: String,
        /// Node name (defaults to the ssh alias)
        #[arg(long)]
        name: Option<String>,
        /// Roles for this node (comma-separated). Defaults to `live`.
        #[arg(long, value_delimiter = ',')]
        role: Vec<String>,
    },
    /// List registered nodes
    List {
        /// Show only the cached node registry without checking Docker/Verglas
        #[arg(long)]
        offline: bool,
    },
    /// Remove a node from the registry (does not touch the node)
    Remove {
        /// Node name to remove
        name: String,
    },
    /// Run a command on a registered node over SSH
    Exec {
        /// Node name
        name: String,
        /// Command to run on the node (after `--`)
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
    /// Install the host CLI, config, and pull the engine image onto a node
    Install {
        /// Node name
        name: String,
        /// Release tag for the bundles (defaults to the cloud RC tag)
        #[arg(long)]
        release_tag: Option<String>,
    },
    /// Snapshot a strategy to a node and place a live container there
    Deploy {
        /// Node name
        name: String,
        /// Local strategy directory (or path to its main.py)
        strategy_dir: PathBuf,
        /// Live brokerage used for order execution
        #[arg(long)]
        brokerage: String,
        /// Live market-data provider serving strategy market subscriptions
        #[arg(long)]
        live_data_feed: String,
    },
    /// Show live deployment status (one node, or aggregated across all nodes)
    Status {
        /// Node name (omit to aggregate all recorded deployments)
        name: Option<String>,
        /// Deploy id (defaults to the node's single recorded deployment)
        #[arg(long)]
        deploy_id: Option<String>,
    },
    /// Print a node deployment's logs
    Logs {
        /// Node name
        name: String,
        /// Deploy id (defaults to the node's single recorded deployment)
        #[arg(long)]
        deploy_id: Option<String>,
        /// Number of trailing log lines to print
        #[arg(long, default_value_t = 250)]
        lines: usize,
    },
    /// Print a node deployment's portfolio snapshot
    Portfolio {
        /// Node name
        name: String,
        /// Deploy id (defaults to the node's single recorded deployment)
        #[arg(long)]
        deploy_id: Option<String>,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run(args: CloudArgs) -> Result<()> {
    let exec = SshExec;
    match args.command {
        CloudCommand::AddNode {
            ssh_alias,
            name,
            role,
        } => {
            let mut reg = NodeRegistry::load()?;
            let node = cmd_add_node(&exec, &mut reg, &ssh_alias, name, role)?;
            reg.save()?;
            println!(
                "Added node '{}' ({}) roles: {}",
                node.name,
                node.platform,
                node.roles.join(", ")
            );
            Ok(())
        }
        CloudCommand::List { offline } => {
            let mut reg = NodeRegistry::load()?;
            let probe = !offline;
            let rows = cmd_list(&exec, &mut reg, probe)?;
            if probe {
                reg.save()?;
            }
            print_list(&rows, probe);
            Ok(())
        }
        CloudCommand::Remove { name } => {
            let mut reg = NodeRegistry::load()?;
            let removed = reg.remove(&name)?;
            reg.save()?;
            if removed {
                println!("Removed node '{name}'");
            } else {
                println!("No node named '{name}'");
            }
            Ok(())
        }
        CloudCommand::Exec { name, cmd } => {
            let reg = NodeRegistry::load()?;
            cmd_exec(&exec, &reg, &name, &cmd)
        }
        CloudCommand::Install {
            name,
            release_tag,
        } => {
            let reg = NodeRegistry::load()?;
            let tag = release_tag.as_deref().unwrap_or(DEFAULT_RELEASE_TAG);
            let summary = cmd_install(&exec, &reg, &name, tag)?;
            print_install_summary(&summary);
            Ok(())
        }
        CloudCommand::Deploy {
            name,
            strategy_dir,
            brokerage,
            live_data_feed,
        } => {
            let node_reg = NodeRegistry::load()?;
            let node = node_reg
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown node '{name}' — run `rlean cloud list`"))?
                .clone();
            let (local_dir, strategy_name) = deploy::resolve_local_strategy(&strategy_dir)?;
            let opts = DeployOptions::from_cli(brokerage, live_data_feed);
            let node_home = probe_node_home(&exec, &node.ssh_dest)?;

            let mut deploy_reg = CloudDeploymentRegistry::load()?;
            let req = DeployRequest {
                node: &node,
                node_home: &node_home,
                local_strategy_dir: &local_dir,
                strategy_name: &strategy_name,
                opts: &opts,
                now: Utc::now(),
            };
            let summary = cmd_deploy(&exec, &req, &mut deploy_reg)?;
            deploy_reg.save()?;

            println!("Deployed '{strategy_name}' to node '{name}'");
            println!("  deploy id : {}", summary.deploy_id);
            println!("  node dir  : {}", summary.deploy_dir);
            if let Some(pid) = summary.pid {
                println!("  node pid  : {pid}");
            }
            println!("  startup   : {}", summary.startup_status);
            println!(
                "  monitor   : rlean cloud status {name} --deploy-id {}",
                summary.deploy_id
            );
            println!(
                "              rlean cloud logs {name} --deploy-id {}",
                summary.deploy_id
            );
            Ok(())
        }
        CloudCommand::Status { name, deploy_id } => cmd_status(&exec, name, deploy_id),
        CloudCommand::Logs {
            name,
            deploy_id,
            lines,
        } => {
            let reg = CloudDeploymentRegistry::load()?;
            let node = require_node(&name)?;
            let target = resolve_target(&reg, &name, deploy_id.as_deref())?;
            let out = remote_logs(
                &exec,
                &node.ssh_dest,
                &target.strategy_dir,
                &target.deploy_id,
                lines,
            )?;
            print!("{out}");
            Ok(())
        }
        CloudCommand::Portfolio { name, deploy_id } => {
            let reg = CloudDeploymentRegistry::load()?;
            let node = require_node(&name)?;
            let target = resolve_target(&reg, &name, deploy_id.as_deref())?;
            let out = remote_portfolio(
                &exec,
                &node.ssh_dest,
                &target.strategy_dir,
                &target.deploy_id,
            )?;
            print!("{out}");
            Ok(())
        }
    }
}

/// Load a node from the registry by name or error.
fn require_node(name: &str) -> Result<Node> {
    NodeRegistry::load()?
        .get(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown node '{name}' — run `rlean cloud list`"))
}

/// Dispatch `rlean cloud status` across its three modes.
fn cmd_status(
    exec: &dyn RemoteExec,
    name: Option<String>,
    deploy_id: Option<String>,
) -> Result<()> {
    let deploy_reg = CloudDeploymentRegistry::load()?;
    match name {
        // A specific node: exact deploy id, or its single recorded deploy.
        Some(name) => {
            let node = require_node(&name)?;
            match resolve_target(&deploy_reg, &name, deploy_id.as_deref()) {
                Ok(target) => {
                    let out = remote_status(
                        exec,
                        &node.ssh_dest,
                        &target.strategy_dir,
                        &target.deploy_id,
                    )?;
                    print!("{out}");
                    Ok(())
                }
                // No recorded deploy for this node: fall back to listing running
                // deployments on the node directly.
                Err(_) if deploy_id.is_none() => {
                    let out = deploy::remote_list_running(exec, &node.ssh_dest)?;
                    print!("{out}");
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        // No node: aggregate every recorded deployment into one table.
        None => {
            let node_reg = NodeRegistry::load()?;
            let rows = aggregate_status(exec, &node_reg, &deploy_reg);
            println!("{:<16} {:<36} {:<16} PID", "NODE", "DEPLOY ID", "STATUS");
            for r in &rows {
                println!(
                    "{:<16} {:<36} {:<16} {}",
                    r.node, r.deploy_id, r.status, r.pid
                );
            }
            Ok(())
        }
    }
}

fn print_install_summary(summary: &install::InstallSummary) {
    println!(
        "Installed rlean {} on '{}'",
        summary.rlean_version, summary.node
    );
    println!("  version : {}", summary.version_output);
    println!("  node dirs: ~/.local/bin, ~/rlean-cloud/data, ~/rlean-cloud/workspace");
}

// ── Command logic (testable with any RemoteExec) ──────────────────────────────

/// Probe a node over SSH and build a [`Node`] record for the registry.
fn cmd_add_node(
    exec: &dyn RemoteExec,
    reg: &mut NodeRegistry,
    ssh_alias: &str,
    name: Option<String>,
    role: Vec<String>,
) -> Result<Node> {
    // 1. Connectivity check.
    let reach = exec.run(ssh_alias, &["true"])?;
    if reach.status != 0 {
        bail!(
            "cannot reach node over ssh: {ssh_alias} — check ~/.ssh/config\n{}",
            reach.stderr.trim()
        );
    }

    // 2. Probe OS + arch on one line: `uname -s -m` → "Linux aarch64".
    let sm = exec.run(ssh_alias, &["uname", "-s", "-m"])?;
    if sm.status != 0 {
        bail!("failed to probe uname on {ssh_alias}: {}", sm.stderr.trim());
    }
    let line = sm.stdout.trim();
    let mut parts = line.split_whitespace();
    let os_raw = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("empty uname output from {ssh_alias}"))?;
    let arch = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("could not parse arch from uname output '{line}'"))?;

    // 3. Full `uname -a` for the record.
    let una = exec.run(ssh_alias, &["uname", "-a"])?;
    let uname = una.stdout.trim().to_string();

    let platform = platform_triple(os_raw, arch);
    let roles = if role.is_empty() {
        vec!["live".to_string()]
    } else {
        role
    };
    let now = Utc::now().to_rfc3339();

    let node = Node {
        name: name.unwrap_or_else(|| ssh_alias.to_string()),
        ssh_dest: ssh_alias.to_string(),
        os: os_raw.to_lowercase(),
        arch: arch.to_string(),
        platform,
        uname,
        roles,
        added_at: now.clone(),
        last_seen: now,
    };

    reg.add(node.clone())?;
    Ok(node)
}

/// A rendered list row (with optional probe status).
struct ListRow {
    name: String,
    ssh_dest: String,
    platform: String,
    roles: String,
    last_seen: String,
    /// Installed rlean version/reachability, or `None` when not probed.
    rlean_status: Option<String>,
    /// Docker daemon health, or `None` when not probed.
    docker_status: Option<String>,
    /// Verglas gateway reachability from the node, or `None` when not probed.
    verglas_status: Option<String>,
}

/// Build list rows, probing each node when requested.
///
/// On a successful probe the node's `last_seen` is refreshed in `reg`.
fn cmd_list(exec: &dyn RemoteExec, reg: &mut NodeRegistry, probe: bool) -> Result<Vec<ListRow>> {
    let mut rows = Vec::with_capacity(reg.nodes.len());
    let names: Vec<String> = reg.nodes.iter().map(|n| n.name.clone()).collect();

    for name in names {
        let (ssh_dest, platform, roles, last_seen) = {
            let node = reg.get(&name).expect("name came from reg");
            (
                node.ssh_dest.clone(),
                node.platform.clone(),
                node.roles.join(","),
                node.last_seen.clone(),
            )
        };

        let rlean_status = if probe {
            Some(probe_node(exec, &ssh_dest))
        } else {
            None
        };
        let docker_status = if probe {
            Some(probe_docker(exec, &ssh_dest))
        } else {
            None
        };
        let verglas_status = if probe {
            Some(probe_verglas(exec, &ssh_dest))
        } else {
            None
        };

        // Refresh last_seen on a reachable probe.
        let last_seen = if probe && rlean_status.as_deref().map(is_reachable).unwrap_or(false) {
            let now = Utc::now().to_rfc3339();
            if let Some(node) = reg.get_mut(&name) {
                node.last_seen = now.clone();
            }
            now
        } else {
            last_seen
        };

        rows.push(ListRow {
            name,
            ssh_dest,
            platform,
            roles,
            last_seen,
            rlean_status,
            docker_status,
            verglas_status,
        });
    }
    Ok(rows)
}

/// Whether a probe status string represents a reachable node.
fn is_reachable(status: &str) -> bool {
    status != "unreachable" && status != "rlean not installed"
}

/// Probe a single node: run `rlean --version` and classify the result.
fn probe_node(exec: &dyn RemoteExec, ssh_dest: &str) -> String {
    match exec.run(ssh_dest, &["rlean", "--version"]) {
        Ok(out) if out.status == 0 => {
            let v = out.stdout.trim();
            if v.is_empty() {
                "reachable".to_string()
            } else {
                v.to_string()
            }
        }
        Ok(out) if out.status == 127 => "rlean not installed".to_string(),
        // Reachable host but the command failed for another reason: if stderr
        // mentions a missing binary treat it as not installed, else unreachable.
        Ok(out) => {
            if out.stderr.contains("not found") || out.stderr.contains("No such file") {
                "rlean not installed".to_string()
            } else {
                "unreachable".to_string()
            }
        }
        Err(_) => "unreachable".to_string(),
    }
}

fn probe_docker(exec: &dyn RemoteExec, ssh_dest: &str) -> String {
    match exec.run(ssh_dest, &["docker", "info"]) {
        Ok(out) if out.status == 0 => "ok".to_owned(),
        Ok(out) if out.status == 127 || out.stderr.contains("not found") => {
            "not installed".to_owned()
        }
        Ok(_) | Err(_) => "unreachable".to_owned(),
    }
}

/// Remote one-liner used by [`probe_verglas`]. Kept as a const so unit tests can
/// stub the exact argv FakeExec expects.
const VERGLAS_PROBE_SCRIPT: &str = r#"EP="${VERGLAS_ENDPOINT:-}"; if [ -z "$EP" ] && [ -f "$HOME/.rlean/config" ]; then EP=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("verglas_endpoint") or "")' "$HOME/.rlean/config" 2>/dev/null || true); fi; EP="${EP:-http://127.0.0.1:8334}"; CODE=$(curl -fsS --max-time 5 -o /dev/null -w '%{http_code}' "$EP" 2>/dev/null || curl -fsS --max-time 5 -o /dev/null -w '%{http_code}' "$EP/health" 2>/dev/null || echo 000); case "$CODE" in 2*|3*|4*) echo ok;; *) echo unreachable;; esac"#;

fn probe_verglas(exec: &dyn RemoteExec, ssh_dest: &str) -> String {
    match exec.run(ssh_dest, &["sh", "-c", VERGLAS_PROBE_SCRIPT]) {
        Ok(out) if out.status == 0 && out.stdout.trim() == "ok" => "ok".to_owned(),
        Ok(_) | Err(_) => "unreachable".to_owned(),
    }
}

/// Run a command on a registered node, streaming stdout/stderr and propagating
/// a non-zero remote exit status as an error.
fn cmd_exec(exec: &dyn RemoteExec, reg: &NodeRegistry, name: &str, cmd: &[String]) -> Result<()> {
    let node = reg
        .get(name)
        .ok_or_else(|| anyhow::anyhow!("unknown node '{name}' — run `rlean cloud list`"))?;

    let argv: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    let out = exec.run(&node.ssh_dest, &argv)?;

    print!("{}", out.stdout);
    eprint!("{}", out.stderr);

    if out.status != 0 {
        bail!("remote command exited with status {}", out.status);
    }
    Ok(())
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn print_list(rows: &[ListRow], probe: bool) {
    if probe {
        println!(
            "{:<16} {:<20} {:<28} {:<16} {:<26} {:<16} {:<12} VERGLAS",
            "NAME", "SSH_DEST", "PLATFORM", "ROLES", "LAST_SEEN", "RLEAN", "DOCKER"
        );
    } else {
        println!(
            "{:<16} {:<20} {:<28} {:<16} LAST_SEEN",
            "NAME", "SSH_DEST", "PLATFORM", "ROLES"
        );
    }
    for r in rows {
        if probe {
            println!(
                "{:<16} {:<20} {:<28} {:<16} {:<26} {:<16} {:<12} {}",
                r.name,
                r.ssh_dest,
                r.platform,
                r.roles,
                r.last_seen,
                r.rlean_status.as_deref().unwrap_or(""),
                r.docker_status.as_deref().unwrap_or(""),
                r.verglas_status.as_deref().unwrap_or("")
            );
        } else {
            println!(
                "{:<16} {:<20} {:<28} {:<16} {}",
                r.name, r.ssh_dest, r.platform, r.roles, r.last_seen
            );
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use remote::RemoteOutput;
    use std::collections::HashMap;

    /// In-memory [`RemoteExec`] keyed by `(ssh_dest, argv joined by space)`.
    struct FakeExec {
        responses: HashMap<(String, String), RemoteOutput>,
    }

    impl FakeExec {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
            }
        }

        fn set(&mut self, ssh_dest: &str, argv: &[&str], out: RemoteOutput) {
            self.responses
                .insert((ssh_dest.to_string(), argv.join(" ")), out);
        }
    }

    impl RemoteExec for FakeExec {
        fn run(&self, ssh_dest: &str, argv: &[&str]) -> Result<RemoteOutput> {
            let key = (ssh_dest.to_string(), argv.join(" "));
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
    fn add_node_probes_and_records_platform() {
        let mut fake = FakeExec::new();
        fake.set("box", &["true"], ok(""));
        fake.set("box", &["uname", "-s", "-m"], ok("Linux aarch64\n"));
        fake.set(
            "box",
            &["uname", "-a"],
            ok("Linux box 6.1.0 #1 SMP aarch64 GNU/Linux\n"),
        );

        let mut reg = NodeRegistry::default();
        let node = cmd_add_node(&fake, &mut reg, "box", None, vec![]).unwrap();

        assert_eq!(node.name, "box");
        assert_eq!(node.ssh_dest, "box");
        assert_eq!(node.os, "linux");
        assert_eq!(node.arch, "aarch64");
        assert_eq!(node.platform, "aarch64-unknown-linux-gnu");
        assert_eq!(node.roles, vec!["live".to_string()]);
        assert!(node.uname.contains("Linux box"));
        assert_eq!(reg.nodes.len(), 1);
    }

    #[test]
    fn add_node_honors_name_and_roles() {
        let mut fake = FakeExec::new();
        fake.set("m1", &["true"], ok(""));
        fake.set("m1", &["uname", "-s", "-m"], ok("Darwin arm64\n"));
        fake.set("m1", &["uname", "-a"], ok("Darwin mac 24.0.0 arm64\n"));

        let mut reg = NodeRegistry::default();
        let node = cmd_add_node(
            &fake,
            &mut reg,
            "m1",
            Some("mac-runner".to_string()),
            vec!["backtest".to_string(), "live".to_string()],
        )
        .unwrap();

        assert_eq!(node.name, "mac-runner");
        assert_eq!(node.ssh_dest, "m1");
        assert_eq!(node.platform, "aarch64-apple-darwin");
        assert_eq!(node.roles, vec!["backtest".to_string(), "live".to_string()]);
    }

    #[test]
    fn add_node_unreachable_errors() {
        let mut fake = FakeExec::new();
        fake.set(
            "gone",
            &["true"],
            RemoteOutput {
                status: 255,
                stdout: String::new(),
                stderr: "ssh: connect to host gone port 22: No route\n".to_string(),
            },
        );

        let mut reg = NodeRegistry::default();
        let err = cmd_add_node(&fake, &mut reg, "gone", None, vec![]).unwrap_err();
        assert!(err.to_string().contains("cannot reach node over ssh"));
        assert!(reg.nodes.is_empty());
    }

    #[test]
    fn list_probe_marks_reachable_and_updates_last_seen() {
        let mut fake = FakeExec::new();
        fake.set("box", &["true"], ok(""));
        fake.set("box", &["uname", "-s", "-m"], ok("Linux x86_64\n"));
        fake.set("box", &["uname", "-a"], ok("Linux box 6.1.0 x86_64\n"));

        let mut reg = NodeRegistry::default();
        cmd_add_node(&fake, &mut reg, "box", None, vec![]).unwrap();
        // Force a distinguishable prior last_seen.
        reg.get_mut("box").unwrap().last_seen = "1970-01-01T00:00:00+00:00".to_string();

        fake.set("box", &["rlean", "--version"], ok("rlean 0.1.0\n"));
        fake.set("box", &["docker", "info"], ok("Server Version: 27.0\n"));
        fake.set("box", &["sh", "-c", VERGLAS_PROBE_SCRIPT], ok("ok\n"));

        let rows = cmd_list(&fake, &mut reg, true).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rlean_status.as_deref(), Some("rlean 0.1.0"));
        assert_eq!(rows[0].docker_status.as_deref(), Some("ok"));
        assert_eq!(rows[0].verglas_status.as_deref(), Some("ok"));
        // last_seen advanced past the epoch sentinel.
        assert_ne!(
            reg.get("box").unwrap().last_seen,
            "1970-01-01T00:00:00+00:00"
        );
    }

    #[test]
    fn list_probe_reports_not_installed() {
        let mut fake = FakeExec::new();
        fake.set("box", &["true"], ok(""));
        fake.set("box", &["uname", "-s", "-m"], ok("Linux x86_64\n"));
        fake.set("box", &["uname", "-a"], ok("Linux box 6.1.0 x86_64\n"));

        let mut reg = NodeRegistry::default();
        cmd_add_node(&fake, &mut reg, "box", None, vec![]).unwrap();

        fake.set(
            "box",
            &["rlean", "--version"],
            RemoteOutput {
                status: 127,
                stdout: String::new(),
                stderr: "bash: rlean: command not found\n".to_string(),
            },
        );
        fake.set(
            "box",
            &["docker", "info"],
            RemoteOutput {
                status: 127,
                stdout: String::new(),
                stderr: "bash: docker: command not found\n".to_string(),
            },
        );
        fake.set(
            "box",
            &["sh", "-c", VERGLAS_PROBE_SCRIPT],
            ok("unreachable\n"),
        );

        let rows = cmd_list(&fake, &mut reg, true).unwrap();
        assert_eq!(rows[0].rlean_status.as_deref(), Some("rlean not installed"));
        assert_eq!(rows[0].docker_status.as_deref(), Some("not installed"));
        assert_eq!(rows[0].verglas_status.as_deref(), Some("unreachable"));
    }

    #[test]
    fn list_probe_reports_docker_unreachable() {
        let mut fake = FakeExec::new();
        fake.set("box", &["true"], ok(""));
        fake.set("box", &["uname", "-s", "-m"], ok("Linux x86_64\n"));
        fake.set("box", &["uname", "-a"], ok("Linux box 6.1.0 x86_64\n"));

        let mut reg = NodeRegistry::default();
        cmd_add_node(&fake, &mut reg, "box", None, vec![]).unwrap();
        fake.set("box", &["rlean", "--version"], ok("rlean 0.1.1\n"));
        fake.set(
            "box",
            &["docker", "info"],
            RemoteOutput {
                status: 1,
                stdout: String::new(),
                stderr: "Cannot connect to the Docker daemon\n".to_owned(),
            },
        );
        fake.set("box", &["sh", "-c", VERGLAS_PROBE_SCRIPT], ok("ok\n"));

        let rows = cmd_list(&fake, &mut reg, true).unwrap();
        assert_eq!(rows[0].docker_status.as_deref(), Some("unreachable"));
        assert_eq!(rows[0].verglas_status.as_deref(), Some("ok"));
    }

    #[test]
    fn exec_unknown_node_errors() {
        let fake = FakeExec::new();
        let reg = NodeRegistry::default();
        let err = cmd_exec(&fake, &reg, "nope", &["ls".to_string()]).unwrap_err();
        assert!(err.to_string().contains("unknown node"));
    }

    #[test]
    fn exec_propagates_nonzero_status() {
        let mut fake = FakeExec::new();
        fake.set("box", &["true"], ok(""));
        fake.set("box", &["uname", "-s", "-m"], ok("Linux x86_64\n"));
        fake.set("box", &["uname", "-a"], ok("Linux box x86_64\n"));

        let mut reg = NodeRegistry::default();
        cmd_add_node(&fake, &mut reg, "box", None, vec![]).unwrap();

        fake.set(
            "box",
            &["false"],
            RemoteOutput {
                status: 3,
                stdout: String::new(),
                stderr: String::new(),
            },
        );

        let err = cmd_exec(&fake, &reg, "box", &["false".to_string()]).unwrap_err();
        assert!(err.to_string().contains("status 3"));
    }
}
