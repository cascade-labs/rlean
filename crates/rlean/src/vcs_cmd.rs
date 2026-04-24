use anyhow::{bail, Result};
use std::path::Path;
use std::process::Command;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct VcsArgs {
    #[command(subcommand)]
    pub command: VcsCommand,
}

#[derive(clap::Subcommand)]
pub enum VcsCommand {
    /// Show working tree status
    Status,

    /// Show recent commits
    Log {
        /// Number of commits to show
        #[arg(short = 'n', long, default_value_t = 10)]
        n: usize,
    },

    /// Show unstaged changes
    Diff,

    /// Configure or show the upstream remote
    Remote(RemoteArgs),

    /// Stage all changes and commit
    Commit {
        /// Commit message
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Commit (if dirty) then push to remote
    Push {
        /// Commit message used when auto-committing dirty changes
        #[arg(short = 'm', long)]
        message: Option<String>,
    },

    /// Pull from remote (regenerates rlean.json if missing)
    Pull,

    /// Commit, pull --rebase, then push (full bidirectional sync)
    Sync {
        /// Commit message used when auto-committing dirty changes
        #[arg(short = 'm', long)]
        message: Option<String>,
    },
}

#[derive(clap::Args)]
pub struct RemoteArgs {
    #[command(subcommand)]
    pub command: RemoteCommand,
}

#[derive(clap::Subcommand)]
pub enum RemoteCommand {
    /// Print the current remote URL
    Get,

    /// Set (or update) the remote URL
    Set {
        /// Git remote URL (SSH or HTTPS)
        url: String,
    },
}

// ── Dispatch ──────────────────────────────────────────────────────────────────

pub fn run_vcs(args: VcsArgs) -> Result<()> {
    require_git()?;
    let workspace = std::env::current_dir()?;

    match args.command {
        VcsCommand::Status => run_git(&["status"], &workspace),
        VcsCommand::Log { n } => run_git(&["log", "--oneline", &format!("-{n}")], &workspace),
        VcsCommand::Diff => run_git(&["diff"], &workspace),
        VcsCommand::Remote(r) => match r.command {
            RemoteCommand::Get => cmd_remote_get(&workspace),
            RemoteCommand::Set { url } => cmd_remote_set(&url, &workspace),
        },
        VcsCommand::Commit { message } => cmd_commit(message.as_deref(), &workspace),
        VcsCommand::Push { message } => cmd_push(message.as_deref(), &workspace),
        VcsCommand::Pull => cmd_pull(&workspace),
        VcsCommand::Sync { message } => cmd_sync(message.as_deref(), &workspace),
    }
}

// ── Subcommand implementations ────────────────────────────────────────────────

fn cmd_remote_get(workspace: &Path) -> Result<()> {
    require_git_repo(workspace)?;
    run_git(&["remote", "get-url", "origin"], workspace)
}

fn cmd_remote_set(url: &str, workspace: &Path) -> Result<()> {
    require_git_repo(workspace)?;
    let has_origin = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_origin {
        run_git(&["remote", "set-url", "origin", url], workspace)?;
    } else {
        run_git(&["remote", "add", "origin", url], workspace)?;
    }
    println!("Remote 'origin' → {url}");
    Ok(())
}

fn cmd_commit(message: Option<&str>, workspace: &Path) -> Result<()> {
    require_git_repo(workspace)?;
    if !is_dirty(workspace)? {
        println!("Nothing to commit — working tree clean.");
        return Ok(());
    }
    run_git(&["add", "-A"], workspace)?;
    let default_msg = timestamp_msg("vcs commit");
    let msg = message.unwrap_or(&default_msg);
    run_git(&["commit", "-m", msg], workspace)
}

fn cmd_push(message: Option<&str>, workspace: &Path) -> Result<()> {
    require_git_repo(workspace)?;
    require_remote(workspace)?;

    if is_dirty(workspace)? {
        run_git(&["add", "-A"], workspace)?;
        let default_msg = timestamp_msg("vcs push");
        let msg = message.unwrap_or(&default_msg);
        run_git(&["commit", "-m", msg], workspace)?;
    }

    push_to_origin(workspace)
}

fn cmd_pull(workspace: &Path) -> Result<()> {
    require_git_repo(workspace)?;
    require_remote(workspace)?;

    if is_dirty(workspace)? {
        bail!(
            "Uncommitted changes present. \
             Run 'rlean vcs push' or 'git stash' first."
        );
    }

    let branch = current_branch(workspace)?;
    run_git(&["pull", "--rebase", "origin", &branch], workspace)?;
    ensure_rlean_json(workspace)?;
    Ok(())
}

fn cmd_sync(message: Option<&str>, workspace: &Path) -> Result<()> {
    require_git_repo(workspace)?;
    require_remote(workspace)?;

    // Auto-commit dirty changes before rebasing.
    if is_dirty(workspace)? {
        run_git(&["add", "-A"], workspace)?;
        let default_msg = timestamp_msg("vcs sync");
        let msg = message.unwrap_or(&default_msg);
        run_git(&["commit", "-m", msg], workspace)?;
    }

    // Pull with rebase so local commits land on top of remote changes.
    let branch = current_branch(workspace)?;
    run_git(&["pull", "--rebase", "origin", &branch], workspace)?;

    // Push everything to remote.
    push_to_origin(workspace)?;

    ensure_rlean_json(workspace)?;
    Ok(())
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Push to origin, using --set-upstream when the branch has no tracking ref yet.
fn push_to_origin(workspace: &Path) -> Result<()> {
    let has_upstream = Command::new("git")
        .args(["rev-parse", "--verify", "@{u}"])
        .current_dir(workspace)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_upstream {
        run_git(&["push"], workspace)
    } else {
        let branch = current_branch(workspace)?;
        run_git(&["push", "--set-upstream", "origin", &branch], workspace)
    }
}

/// Regenerate rlean.json and data/ when absent (e.g. after a fresh clone + pull).
fn ensure_rlean_json(workspace: &Path) -> Result<()> {
    if workspace.join("rlean.json").exists() {
        return Ok(());
    }
    let lang = crate::config::GlobalConfig::load()
        .map(|c| c.default_language)
        .unwrap_or_else(|_| "python".to_string());
    let ws_config = crate::config::WorkspaceConfig {
        data_folder: "data".to_string(),
        default_language: lang,
    };
    ws_config.save(workspace)?;
    std::fs::create_dir_all(workspace.join("data"))?;
    println!("Regenerated rlean.json");
    Ok(())
}

fn require_git() -> Result<()> {
    match Command::new("git").arg("--version").output() {
        Ok(o) if o.status.success() => Ok(()),
        _ => bail!("'git' not found. Install git and re-run."),
    }
}

fn require_git_repo(workspace: &Path) -> Result<()> {
    match Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(workspace)
        .output()
    {
        Ok(o) if o.status.success() => Ok(()),
        _ => bail!("Not a git repository. Run 'rlean init' or 'git init' first."),
    }
}

fn require_remote(workspace: &Path) -> Result<()> {
    let ok = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(workspace)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        bail!("No remote configured. Run 'rlean vcs remote set <url>' first.");
    }
    Ok(())
}

fn is_dirty(workspace: &Path) -> Result<bool> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .output()?;
    Ok(!output.stdout.is_empty())
}

fn current_branch(workspace: &Path) -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(workspace)
        .output()?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn timestamp_msg(prefix: &str) -> String {
    format!(
        "{} {}",
        prefix,
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
    )
}

/// Run a git command with inherited stdio so output goes directly to the terminal.
fn run_git(args: &[&str], workspace: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(args)
        .current_dir(workspace)
        .status()?;
    if !status.success() {
        bail!(
            "git {} failed (exit {:?})",
            args.join(" "),
            status.code()
        );
    }
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timestamp_msg_contains_prefix() {
        let msg = timestamp_msg("vcs push");
        assert!(msg.starts_with("vcs push "), "msg={msg}");
    }

    #[test]
    fn test_timestamp_msg_contains_date() {
        let msg = timestamp_msg("vcs sync");
        // Should contain a date-like string (YYYY-MM-DD)
        assert!(msg.contains('-'), "msg={msg}");
        assert!(msg.len() > 15, "msg={msg}");
    }
}
