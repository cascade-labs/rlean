//! Remote command execution over SSH.
//!
//! [`SshExec`] is the real implementation; command logic depends only on the
//! [`RemoteExec`] trait so it can be unit-tested with an in-memory fake.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::config::rlean_dir;

/// The result of running a command on a remote node.
pub struct RemoteOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a command argv on a remote SSH destination.
pub trait RemoteExec {
    fn run(&self, ssh_dest: &str, argv: &[&str]) -> Result<RemoteOutput>;
}

// ── SSH implementation ────────────────────────────────────────────────────────

/// Real SSH executor using OpenSSH connection multiplexing.
pub struct SshExec;

impl RemoteExec for SshExec {
    fn run(&self, ssh_dest: &str, argv: &[&str]) -> Result<RemoteOutput> {
        let control_path = ssh_control_path()?;
        let args = ssh_command_argv(&control_path, ssh_dest, argv);
        let output = Command::new("ssh")
            .args(&args)
            .output()
            .context("failed to spawn ssh — is OpenSSH installed and on PATH?")?;
        Ok(RemoteOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// The OpenSSH `ControlPath` template under `~/.rlean`.
///
/// Uses the `%C` hash token (a short digest of local host, remote user, host,
/// and port) instead of `%r@%h:%p` — unix socket paths are limited to ~104
/// bytes on macOS and a spelled-out user@host:port can blow past that.
fn ssh_control_path() -> Result<String> {
    let dir = rlean_dir()?;
    // ssh does not create the socket directory itself; without it the
    // ControlMaster bind fails with "cannot bind to path".
    std::fs::create_dir_all(&dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let path = dir.join("ssh-%C");
    Ok(path.to_string_lossy().into_owned())
}

/// Build the full `ssh` argument vector with connection multiplexing options.
///
/// Extracted so the argv can be unit-tested without invoking `ssh`.
pub(crate) fn ssh_command_argv(control_path: &str, ssh_dest: &str, argv: &[&str]) -> Vec<String> {
    let mut args = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=10".to_string(),
        "-o".to_string(),
        "ControlMaster=auto".to_string(),
        "-o".to_string(),
        "ControlPersist=60s".to_string(),
        "-o".to_string(),
        format!("ControlPath={control_path}"),
        ssh_dest.to_string(),
        "--".to_string(),
    ];
    args.extend(argv.iter().map(|a| a.to_string()));
    args
}

// ── File transfer (scp / rsync) ────────────────────────────────────────────────

/// The set of `-o Key=Value` OpenSSH options shared by ssh, scp, and rsync so
/// every transfer reuses the same multiplexed control connection.
fn ssh_control_options(control_path: &str) -> [(&'static str, String); 5] {
    [
        ("BatchMode", "yes".to_string()),
        ("ConnectTimeout", "10".to_string()),
        ("ControlMaster", "auto".to_string()),
        ("ControlPersist", "60s".to_string()),
        ("ControlPath", control_path.to_string()),
    ]
}

/// Build the `scp` argument vector to copy a single local file to
/// `ssh_dest:remote_path`, reusing the shared multiplexing options.
///
/// Extracted so the argv can be unit-tested without invoking `scp`.
pub(crate) fn scp_to_argv(
    control_path: &str,
    ssh_dest: &str,
    local: &str,
    remote_path: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    for (key, value) in ssh_control_options(control_path) {
        args.push("-o".to_string());
        args.push(format!("{key}={value}"));
    }
    // `--` separates options from the source/target operands so paths that
    // begin with `-` are never treated as flags.
    args.push("--".to_string());
    args.push(local.to_string());
    args.push(format!("{ssh_dest}:{remote_path}"));
    args
}

/// Build the `rsync` argument vector to mirror `local_dir` into
/// `ssh_dest:remote_dir`, reusing the shared multiplexing options for the
/// underlying ssh transport and applying `--delete` plus the given excludes.
///
/// Extracted so the argv can be unit-tested without invoking `rsync`.
pub(crate) fn rsync_to_argv(
    control_path: &str,
    ssh_dest: &str,
    local_dir: &str,
    remote_dir: &str,
    extra_excludes: &[&str],
) -> Vec<String> {
    // Compose the `-e "ssh -o ..."` transport string from the shared options.
    let mut ssh_cmd = String::from("ssh");
    for (key, value) in ssh_control_options(control_path) {
        ssh_cmd.push_str(&format!(" -o {key}={value}"));
    }

    let mut args = vec![
        "-az".to_string(),
        "--delete".to_string(),
        "-e".to_string(),
        ssh_cmd,
    ];
    for exclude in extra_excludes {
        args.push("--exclude".to_string());
        args.push((*exclude).to_string());
    }
    // Trailing slash on the source copies the directory *contents* into
    // remote_dir rather than nesting it.
    let src = if local_dir.ends_with('/') {
        local_dir.to_string()
    } else {
        format!("{local_dir}/")
    };
    args.push("--".to_string());
    args.push(src);
    args.push(format!("{ssh_dest}:{remote_dir}"));
    args
}

/// scp a single local file to `ssh_dest:remote_path`.
pub(crate) fn scp_to(ssh_dest: &str, local: &Path, remote_path: &str) -> Result<()> {
    let control_path = ssh_control_path()?;
    let local = local.to_string_lossy();
    let args = scp_to_argv(&control_path, ssh_dest, &local, remote_path);
    let output = Command::new("scp")
        .args(&args)
        .output()
        .context("failed to spawn scp — is OpenSSH installed and on PATH?")?;
    if !output.status.success() {
        anyhow::bail!(
            "scp {local} → {ssh_dest}:{remote_path} failed (status {}):\n{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// rsync `local_dir` into `ssh_dest:remote_dir` with `--delete` and excludes.
pub(crate) fn rsync_to(
    ssh_dest: &str,
    local_dir: &Path,
    remote_dir: &str,
    extra_excludes: &[&str],
) -> Result<()> {
    let control_path = ssh_control_path()?;
    let local = local_dir.to_string_lossy();
    let args = rsync_to_argv(&control_path, ssh_dest, &local, remote_dir, extra_excludes);
    let output = Command::new("rsync")
        .args(&args)
        .output()
        .context("failed to spawn rsync — is rsync installed and on PATH?")?;
    if !output.status.success() {
        anyhow::bail!(
            "rsync {local} → {ssh_dest}:{remote_dir} failed (status {}):\n{}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_command_argv_contains_multiplexing_options_in_order() {
        let args = ssh_command_argv("/home/u/.rlean/ssh-%C", "node1", &["uname", "-s"]);

        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ControlMaster=auto".to_string()));
        assert!(args.contains(&"ControlPersist=60s".to_string()));
        assert!(args.contains(&"ControlPath=/home/u/.rlean/ssh-%C".to_string()));

        // ssh_dest, then "--", then the passed argv, in that order.
        let dest = args.iter().position(|a| a == "node1").unwrap();
        let sep = args.iter().position(|a| a == "--").unwrap();
        let uname = args.iter().position(|a| a == "uname").unwrap();
        let flag = args.iter().position(|a| a == "-s").unwrap();
        assert!(dest < sep);
        assert!(sep < uname);
        assert!(uname < flag);
    }

    #[test]
    fn scp_argv_uses_shared_options_and_dest_target() {
        let args = scp_to_argv(
            "/home/u/.rlean/ssh-%C",
            "opc@node",
            "/tmp/rlean.bin",
            "/home/opc/.local/bin/rlean.new",
        );

        // Shared multiplexing options are present in `-o Key=Value` form.
        assert!(args.contains(&"BatchMode=yes".to_string()));
        assert!(args.contains(&"ControlMaster=auto".to_string()));
        assert!(args.contains(&"ControlPersist=60s".to_string()));
        assert!(args.contains(&"ControlPath=/home/u/.rlean/ssh-%C".to_string()));

        // Operands come after `--`, source before dest, dest is user@host:path.
        let sep = args.iter().position(|a| a == "--").unwrap();
        let src = args.iter().position(|a| a == "/tmp/rlean.bin").unwrap();
        let dst = args
            .iter()
            .position(|a| a == "opc@node:/home/opc/.local/bin/rlean.new")
            .unwrap();
        assert!(sep < src);
        assert!(src < dst);
    }

    #[test]
    fn rsync_argv_builds_ssh_transport_and_excludes() {
        let args = rsync_to_argv(
            "/home/u/.rlean/ssh-%C",
            "opc@node",
            "/local/strategy",
            "/home/opc/rlean-cloud/workspace/strategy",
            &["backtests/", "__pycache__/"],
        );

        // Archive + delete for a faithful working-tree snapshot.
        assert!(args.contains(&"-az".to_string()));
        assert!(args.contains(&"--delete".to_string()));

        // The `-e` transport string carries the shared multiplexing options.
        let e = args.iter().position(|a| a == "-e").unwrap();
        let ssh_cmd = &args[e + 1];
        assert!(ssh_cmd.starts_with("ssh "));
        assert!(ssh_cmd.contains("-o ControlPath=/home/u/.rlean/ssh-%C"));
        assert!(ssh_cmd.contains("-o BatchMode=yes"));

        // Excludes are passed through.
        assert!(args.contains(&"--exclude".to_string()));
        assert!(args.contains(&"backtests/".to_string()));
        assert!(args.contains(&"__pycache__/".to_string()));

        // Source gets a trailing slash (copy contents); dest is user@host:dir.
        assert!(args.contains(&"/local/strategy/".to_string()));
        assert!(args.contains(&"opc@node:/home/opc/rlean-cloud/workspace/strategy".to_string()));
    }

    #[test]
    fn rsync_argv_preserves_existing_trailing_slash() {
        let args = rsync_to_argv("/cp", "node", "/local/strategy/", "/remote/strategy", &[]);
        // Should not double the slash.
        assert!(args.contains(&"/local/strategy/".to_string()));
        assert!(!args.contains(&"/local/strategy//".to_string()));
    }
}
