/// `rlean registry` — manage plugin registries
///
/// Registries are JSON manifests that list available plugins.  The official
/// rlean registry is always active.  Add private or local registries to access
/// proprietary plugins.
///
/// Registry files must follow the rlean registry.json schema:
///   { "version": "1", "plugins": [ { "name", "version", "kind",
///                                     "description", "git_url", "subdir?" } ] }
///
/// Usage:
///   rlean registry list                                  # show configured registries
///   rlean registry add <url>                             # add a registry
///   rlean registry remove <url>                          # remove a registry
///
/// Supported registry URL schemes:
///   https://raw.githubusercontent.com/org/repo/main/registry.json  # hosted
///   file:///path/to/cascadelabs-plugins/registry.json               # local dev
///   /absolute/path/to/registry.json                                 # local dev
use anyhow::{bail, Result};

use crate::plugin_cmd::{load_user_registries, save_user_registries, OFFICIAL_REGISTRY_URL};

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct RegistryArgs {
    #[command(subcommand)]
    pub command: RegistryCommand,
}

#[derive(clap::Subcommand)]
pub enum RegistryCommand {
    /// List all configured registries
    List,
    /// Add a registry (https://, file://, or /absolute/path/registry.json)
    Add {
        /// Registry URL or absolute path to a registry.json file
        url: String,
    },
    /// Remove a registry
    Remove {
        /// Registry URL to remove (must match exactly as added)
        url: String,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_registry(args: RegistryArgs) -> Result<()> {
    match args.command {
        RegistryCommand::List => cmd_list(),
        RegistryCommand::Add { url } => cmd_add(&url),
        RegistryCommand::Remove { url } => cmd_remove(&url),
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_list() -> Result<()> {
    let user = load_user_registries()?;
    println!("{:<10} URL", "TYPE");
    println!("{}", "-".repeat(80));
    println!("{:<10} {}", "built-in", OFFICIAL_REGISTRY_URL);
    for url in &user.urls {
        println!("{:<10} {}", "user", url);
    }
    Ok(())
}

fn cmd_add(url: &str) -> Result<()> {
    let url = normalize_registry_url(url);
    let url = url.as_str();

    if url == OFFICIAL_REGISTRY_URL {
        bail!("That is the built-in registry — it is always active and cannot be re-added.");
    }
    let mut user = load_user_registries()?;
    if user.urls.iter().any(|u| u == url) {
        bail!("Registry '{}' is already configured.", url);
    }
    user.urls.push(url.to_string());
    save_user_registries(&user)?;
    println!("Added registry: {url}");
    println!("Run `rlean plugin list` to see available plugins.");
    Ok(())
}

/// Convert a bare GitHub repo URL to the raw registry.json URL.
///
/// `https://github.com/org/repo`  →  `https://raw.githubusercontent.com/org/repo/main/registry.json`
///
/// URLs that already point at raw content, local files, or other hosts are
/// returned unchanged.
fn normalize_registry_url(url: &str) -> String {
    // Match exactly https://github.com/{owner}/{repo} (no trailing path beyond the repo name)
    if let Some(rest) = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
    {
        // rest is "{owner}/{repo}" with at most one slash — reject if it has more segments
        let parts: Vec<&str> = rest.trim_end_matches('/').splitn(3, '/').collect();
        if parts.len() == 2 {
            let (owner, repo) = (parts[0], parts[1]);
            let raw = format!(
                "https://raw.githubusercontent.com/{}/{}/main/registry.json",
                owner, repo
            );
            eprintln!("Note: converted GitHub repo URL to raw registry URL:\n  {raw}");
            return raw;
        }
    }
    url.to_string()
}

fn cmd_remove(url: &str) -> Result<()> {
    let normalized = normalize_registry_url(url);
    let normalized = normalized.as_str();

    if normalized == OFFICIAL_REGISTRY_URL || url == OFFICIAL_REGISTRY_URL {
        bail!("The built-in registry cannot be removed.");
    }
    let mut user = load_user_registries()?;
    let before = user.urls.len();
    // Match either the normalized form or the original input (handles legacy entries
    // that were stored before URL normalization was introduced).
    user.urls.retain(|u| u != normalized && u != url);
    if user.urls.len() == before {
        bail!("Registry '{}' is not configured. Run `rlean registry list` to see configured registries.", url);
    }
    save_user_registries(&user)?;
    println!("Removed registry: {url}");
    Ok(())
}
