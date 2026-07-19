use anyhow::Result;
use std::path::Path;
use std::process::Command;

use crate::config::{GlobalConfig, WorkspaceConfig};

#[derive(clap::Args)]
pub struct InitArgs {
    /// Default strategy language
    #[arg(long, default_value = "python", value_parser = ["python", "csharp"])]
    pub language: String,
}

pub fn run_init(args: InitArgs) -> Result<()> {
    let workspace = std::env::current_dir()?;

    // ── rlean.json ─────────────────────────────────────────────────────────────
    // rlean.json is excluded from git (host-specific). Always regenerate when
    // missing so that `git clone` + `rlean init` leaves the workspace ready.
    let json_path = workspace.join("rlean.json");
    if json_path.exists() {
        println!("rlean.json already exists — skipping");
    } else {
        let ws_config = WorkspaceConfig {
            default_language: args.language.clone(),
        };
        ws_config.save(&workspace)?;
        println!("Created rlean.json");
    }

    // ── ~/.rlean/config ───────────────────────────────────────────────────────
    let mut global = GlobalConfig::load()?;
    global.default_language = args.language.clone();
    global.workspace = Some(workspace.display().to_string());
    global.save()?;
    println!("Updated ~/.rlean/config");

    // ── git ───────────────────────────────────────────────────────────────────
    if git_available() {
        setup_git(&workspace)?;
    } else {
        eprintln!(
            "Warning: 'git' not found — skipping VCS setup. \
             Install git to enable version control."
        );
    }

    println!();
    println!("Workspace ready at {}", workspace.display());

    if git_available() && workspace.join(".git").exists() {
        let has_remote = Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(&workspace)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !has_remote {
            println!("Tip: set a remote with 'rlean vcs remote set <url>'");
        }
    }

    println!("Next: rlean create-project <name>");

    Ok(())
}

// ── Git helpers ───────────────────────────────────────────────────────────────

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Idempotent git setup: init repo, write .gitignore, make initial commit.
fn setup_git(workspace: &Path) -> Result<()> {
    // git init — skip if .git already exists
    if !workspace.join(".git").exists() {
        let status = Command::new("git")
            .arg("init")
            .current_dir(workspace)
            .status()?;
        if status.success() {
            println!("Initialized git repository");
        }
    }

    ensure_gitignore(workspace)?;

    // Initial commit — only when the repo has no commits yet
    if !git_has_commits(workspace) {
        Command::new("git")
            .args(["add", "-A"])
            .current_dir(workspace)
            .status()?;

        if has_staged_changes(workspace) {
            let status = Command::new("git")
                .args(["commit", "-m", "rlean init"])
                .current_dir(workspace)
                .status()?;
            if status.success() {
                println!("Created initial commit");
            }
        }
    }

    Ok(())
}

/// Write (or update) .gitignore, appending only the entries that are missing.
pub(crate) fn ensure_gitignore(workspace: &Path) -> Result<()> {
    let path = workspace.join(".gitignore");
    let existed = path.exists();
    let existing = if existed {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };

    let sections: &[(&str, &[&str])] = &[
        ("# rlean workspace — host-specific config", &["rlean.json"]),
        (
            "# Generated output",
            &["**/backtests/", "**/live/", "**/.runs/", "**/.weco/"],
        ),
        ("# Python artifacts", &["__pycache__/", "*.pyc", "*.pyo"]),
        ("# macOS", &[".DS_Store"]),
        ("# Secrets", &[".env", "*.env"]),
    ];

    let mut additions: Vec<String> = Vec::new();
    for &(comment, patterns) in sections {
        let missing: Vec<&str> = patterns
            .iter()
            .copied()
            .filter(|p| !existing.lines().any(|l| l.trim() == *p))
            .collect();
        if !missing.is_empty() {
            additions.push(comment.to_string());
            for p in missing {
                additions.push(p.to_string());
            }
            additions.push(String::new());
        }
    }

    if additions.is_empty() {
        return Ok(());
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    if !content.is_empty() {
        content.push('\n');
    }
    for line in &additions {
        content.push_str(line);
        content.push('\n');
    }

    std::fs::write(&path, content)?;
    if existed {
        println!("Updated .gitignore");
    } else {
        println!("Created .gitignore");
    }
    Ok(())
}

fn git_has_commits(workspace: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(workspace)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns true when `git diff --cached` reports staged changes (exit 1).
fn has_staged_changes(workspace: &Path) -> bool {
    Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(workspace)
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_gitignore_adds_generated_tool_directories_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();

        ensure_gitignore(dir.path()).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(contents.lines().any(|line| line == "**/.runs/"));
        assert!(contents.lines().any(|line| line == "**/.weco/"));
    }

    #[test]
    fn ensure_gitignore_does_not_duplicate_existing_tool_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "**/.runs/\n**/.weco/\n").unwrap();

        ensure_gitignore(dir.path()).unwrap();

        let contents = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(
            contents.lines().filter(|line| *line == "**/.runs/").count(),
            1
        );
        assert_eq!(
            contents.lines().filter(|line| *line == "**/.weco/").count(),
            1
        );
    }
}
