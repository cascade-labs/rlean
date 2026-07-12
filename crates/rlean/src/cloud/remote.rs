//! Remote command execution over SSH.
//!
//! [`SshExec`] is the real implementation; command logic depends only on the
//! [`RemoteExec`] trait so it can be unit-tested with an in-memory fake.

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
}
