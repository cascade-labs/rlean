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
        RegistryCommand::List           => cmd_list(),
        RegistryCommand::Add { url }    => cmd_add(&url),
        RegistryCommand::Remove { url } => cmd_remove(&url),
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_list() -> Result<()> {
    let user = load_user_registries()?;
    println!("{:<10} {}", "TYPE", "URL");
    println!("{}", "-".repeat(80));
    println!("{:<10} {}", "built-in", OFFICIAL_REGISTRY_URL);
    for url in &user.urls {
        println!("{:<10} {}", "user", url);
    }
    Ok(())
}

fn cmd_add(url: &str) -> Result<()> {
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

fn cmd_remove(url: &str) -> Result<()> {
    if url == OFFICIAL_REGISTRY_URL {
        bail!("The built-in registry cannot be removed.");
    }
    let mut user = load_user_registries()?;
    let before = user.urls.len();
    user.urls.retain(|u| u != url);
    if user.urls.len() == before {
        bail!("Registry '{}' is not configured. Run `rlean registry list` to see configured registries.", url);
    }
    save_user_registries(&user)?;
    println!("Removed registry: {url}");
    Ok(())
}

