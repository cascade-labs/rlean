/// `rlean plugin` — plugin registry management
///
/// Plugins are `cdylib` crates that export `rlean_plugin_descriptor()`.
/// Installed plugins live in ~/.rlean/plugins/ as .dylib/.so files.
/// The manifest at ~/.rlean/plugins.json tracks what is installed.
///
/// Usage:
///   rlean plugin list                  # show available plugins from registry
///   rlean plugin list --installed      # show installed plugins
///   rlean plugin install <name>        # clone + build + install
///   rlean plugin upgrade <name>        # rebuild from updated source
///   rlean plugin remove <name>         # remove installed plugin
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(clap::Subcommand)]
pub enum PluginCommand {
    /// List available plugins from the registry (pass --installed for local)
    List {
        /// Show only installed plugins
        #[arg(long)]
        installed: bool,
    },
    /// Clone, build, and install a plugin
    Install {
        /// Plugin name (from registry) or a git URL
        name: String,
    },
    /// Rebuild an installed plugin from its latest source
    Upgrade {
        /// Plugin name
        name: String,
    },
    /// Remove an installed plugin
    Remove {
        /// Plugin name
        name: String,
    },
}

// ── Registry entry (remote + local manifest share this shape) ─────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name:        String,
    pub version:     String,
    pub kind:        String,
    pub description: String,
    /// Git URL used to clone + build the plugin
    pub git_url:     String,
    /// Optional: subdirectory inside the repo containing the plugin crate
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir:      Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InstalledManifest {
    plugins: Vec<InstalledEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstalledEntry {
    #[serde(flatten)]
    pub info:         RegistryEntry,
    pub installed_at: String,
    /// Absolute path to the compiled .dylib/.so
    pub lib_path:     String,
    /// Checkout path (for upgrade)
    pub src_path:     String,
}

// ── Built-in registry ─────────────────────────────────────────────────────────
//
// Until there is a hosted registry URL this is the canonical plugin list.
// Add entries here (or later fetch from a remote JSON).

fn builtin_registry() -> Vec<RegistryEntry> {
    vec![
        RegistryEntry {
            name:        "tradier".to_string(),
            version:     "0.1.0".to_string(),
            kind:        "brokerage".to_string(),
            description: "Tradier brokerage — equities and options order routing".to_string(),
            git_url:     "https://github.com/cascade-labs/rlean-plugin-tradier".to_string(),
            subdir:      None,
        },
        RegistryEntry {
            name:        "kalshi".to_string(),
            version:     "0.1.0".to_string(),
            kind:        "brokerage,data-provider-live".to_string(),
            description: "Kalshi prediction market brokerage and live data feed".to_string(),
            git_url:     "https://github.com/cascade-labs/rlean-plugin-kalshi".to_string(),
            subdir:      None,
        },
        RegistryEntry {
            name:        "openai".to_string(),
            version:     "0.1.0".to_string(),
            kind:        "ai-skill".to_string(),
            description: "OpenAI GPT integration for signal generation and summarisation".to_string(),
            git_url:     "https://github.com/cascade-labs/rlean-plugin-openai".to_string(),
            subdir:      None,
        },
        RegistryEntry {
            name:        "fred".to_string(),
            version:     "0.1.0".to_string(),
            kind:        "custom-data".to_string(),
            description: "FRED macroeconomic data source (Federal Reserve Economic Data)".to_string(),
            git_url:     "https://github.com/cascade-labs/rlean-plugin-fred".to_string(),
            subdir:      None,
        },
        RegistryEntry {
            name:        "massive".to_string(),
            version:     "0.1.0".to_string(),
            kind:        "data-provider-historical".to_string(),
            description: "Massive.com (formerly Polygon.io) historical data provider".to_string(),
            git_url:     "https://github.com/cascade-labs/rlean-plugins".to_string(),
            subdir:      Some("massive".to_string()),
        },
        RegistryEntry {
            name:        "thetadata".to_string(),
            version:     "0.1.0".to_string(),
            kind:        "data-provider-historical".to_string(),
            description: "ThetaData options and equity historical data provider".to_string(),
            git_url:     "https://github.com/cascade-labs/rlean-plugins".to_string(),
            subdir:      Some("thetadata".to_string()),
        },
    ]
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_plugin(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List { installed } => {
            if installed { cmd_list_installed() } else { cmd_list_registry() }
        }
        PluginCommand::Install { name } => cmd_install(&name),
        PluginCommand::Upgrade { name }  => cmd_upgrade(&name),
        PluginCommand::Remove  { name }  => cmd_remove(&name),
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_list_registry() -> Result<()> {
    let registry = builtin_registry();
    let installed = load_manifest()?.plugins;
    let installed_names: std::collections::HashSet<&str> =
        installed.iter().map(|e| e.info.name.as_str()).collect();

    println!("{:<16} {:<30} {:<28} {}", "NAME", "DESCRIPTION", "KIND", "STATUS");
    println!("{}", "-".repeat(90));
    for entry in &registry {
        let status = if installed_names.contains(entry.name.as_str()) {
            "installed"
        } else {
            ""
        };
        println!(
            "{:<16} {:<30} {:<28} {}",
            entry.name,
            truncate(&entry.description, 29),
            entry.kind,
            status
        );
    }
    println!();
    println!("Install:  rlean plugin install <name>");
    Ok(())
}

fn cmd_list_installed() -> Result<()> {
    let manifest = load_manifest()?;
    if manifest.plugins.is_empty() {
        println!("No plugins installed. Run `rlean plugin list` to see available plugins.");
        return Ok(());
    }
    println!("{:<16} {:<10} {:<28} {}", "NAME", "VERSION", "KIND", "INSTALLED");
    println!("{}", "-".repeat(72));
    for entry in &manifest.plugins {
        println!(
            "{:<16} {:<10} {:<28} {}",
            entry.info.name, entry.info.version, entry.info.kind, entry.installed_at
        );
    }
    Ok(())
}

fn cmd_install(name: &str) -> Result<()> {
    // Accept a full git URL or a short registry name.
    let entry = resolve_registry_entry(name)?;

    // Check not already installed.
    let mut manifest = load_manifest()?;
    if manifest.plugins.iter().any(|e| e.info.name == entry.name) {
        bail!("Plugin '{}' is already installed. Use `rlean plugin upgrade {}` to update.", name, name);
    }

    let src_dir  = plugin_src_dir(&entry.name)?;
    let lib_path = plugin_lib_path(&entry.name)?;

    // Clone
    println!("Cloning {} ...", entry.git_url);
    git_clone(&entry.git_url, &src_dir)?;

    // Build — always run from the workspace/repo root so path resolution is correct.
    // Use `-p` to build only this plugin crate even if the repo is a workspace.
    println!("Building {} ...", entry.name);
    let package_name = format!("rlean-plugin-{}", entry.name);
    cargo_build(&src_dir, &package_name)?;

    // The compiled library is always in the workspace root's target/, not the subdir's.
    let built = find_built_lib(&src_dir, &entry.name)?;
    std::fs::copy(&built, &lib_path)
        .with_context(|| format!("Failed to copy {} → {}", built.display(), lib_path.display()))?;

    // Update manifest
    manifest.plugins.push(InstalledEntry {
        info:         entry.clone(),
        installed_at: now_utc(),
        lib_path:     lib_path.display().to_string(),
        src_path:     src_dir.display().to_string(),
    });
    save_manifest(&manifest)?;

    println!("Installed '{}'  →  {}", entry.name, lib_path.display());
    Ok(())
}

fn cmd_upgrade(name: &str) -> Result<()> {
    let mut manifest = load_manifest()?;
    let idx = manifest.plugins.iter().position(|e| e.info.name == name)
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' is not installed.", name))?;

    let src_dir = PathBuf::from(&manifest.plugins[idx].src_path);
    let lib_path = PathBuf::from(&manifest.plugins[idx].lib_path);
    let subdir = manifest.plugins[idx].info.subdir.clone();

    // Pull latest
    println!("Pulling latest source for '{}' ...", name);
    git_pull(&src_dir)?;

    // Rebuild from workspace root with -p flag.
    println!("Building '{}' ...", name);
    let package_name = format!("rlean-plugin-{}", name);
    cargo_build(&src_dir, &package_name)?;

    let built = find_built_lib(&src_dir, name)?;
    std::fs::copy(&built, &lib_path)
        .with_context(|| format!("Failed to copy {} → {}", built.display(), lib_path.display()))?;

    manifest.plugins[idx].installed_at = now_utc();
    save_manifest(&manifest)?;

    println!("Upgraded '{}'.", name);
    Ok(())
}

fn cmd_remove(name: &str) -> Result<()> {
    let mut manifest = load_manifest()?;
    let idx = manifest.plugins.iter().position(|e| e.info.name == name)
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' is not installed.", name))?;

    let lib_path = PathBuf::from(&manifest.plugins[idx].lib_path);
    if lib_path.exists() {
        std::fs::remove_file(&lib_path)
            .with_context(|| format!("Failed to remove {}", lib_path.display()))?;
    }

    manifest.plugins.remove(idx);
    save_manifest(&manifest)?;

    println!("Removed '{}'.", name);
    Ok(())
}

// ── Paths ─────────────────────────────────────────────────────────────────────

fn plugins_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    let dir = home.join(".rlean").join("plugins");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn plugin_src_dir(name: &str) -> Result<PathBuf> {
    let home = home_dir()?;
    let dir = home.join(".rlean").join("plugin-src").join(name);
    std::fs::create_dir_all(dir.parent().unwrap())?;
    Ok(dir)
}

fn plugin_lib_path(name: &str) -> Result<PathBuf> {
    // Must match the pattern providers.rs uses to look up installed plugins:
    //   librlean_plugin_<name>.<dylib|so>
    let lib_name = format!("librlean_plugin_{}.{}", name.replace('-', "_"), dylib_ext());
    Ok(plugins_dir()?.join(lib_name))
}

fn manifest_path() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".rlean").join("plugins.json"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(PathBuf::from))
        .context("HOME env not set")
}

fn dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") { "dylib" } else { "so" }
}

// ── Manifest I/O ──────────────────────────────────────────────────────────────

fn load_manifest() -> Result<InstalledManifest> {
    let path = manifest_path()?;
    if !path.exists() {
        return Ok(InstalledManifest::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).context("Failed to parse plugins.json")
}

fn save_manifest(manifest: &InstalledManifest) -> Result<()> {
    let path = manifest_path()?;
    let text = serde_json::to_string_pretty(manifest)?;
    std::fs::write(&path, text)?;
    Ok(())
}

// ── Registry lookup ───────────────────────────────────────────────────────────

fn resolve_registry_entry(name_or_url: &str) -> Result<RegistryEntry> {
    // If it looks like a URL, synthesise an entry from it.
    if name_or_url.starts_with("http") {
        let name = name_or_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(name_or_url)
            .trim_start_matches("rlean-plugin-")
            .to_string();
        return Ok(RegistryEntry {
            name:        name.clone(),
            version:     "unknown".to_string(),
            kind:        "unknown".to_string(),
            description: String::new(),
            git_url:     name_or_url.to_string(),
            subdir:      None,
        });
    }

    builtin_registry()
        .into_iter()
        .find(|e| e.name == name_or_url)
        .ok_or_else(|| anyhow::anyhow!(
            "Plugin '{}' not found in registry.\n\
             Run `rlean plugin list` to see available plugins, or pass a git URL directly.",
            name_or_url
        ))
}

// ── Git + Cargo helpers ───────────────────────────────────────────────────────

fn git_clone(url: &str, dest: &Path) -> Result<()> {
    if dest.exists() {
        bail!(
            "Source directory '{}' already exists. Remove it first or use `rlean plugin upgrade`.",
            dest.display()
        );
    }
    let status = Command::new("git")
        .args(["clone", "--depth=1", url, &dest.display().to_string()])
        .status()
        .context("Failed to run git clone — is git installed?")?;
    if !status.success() {
        bail!("git clone failed for {}", url);
    }
    Ok(())
}

fn git_pull(dir: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["-C", &dir.display().to_string(), "pull", "--ff-only"])
        .status()
        .context("Failed to run git pull")?;
    if !status.success() {
        bail!("git pull failed in {}", dir.display());
    }
    Ok(())
}

fn cargo_build(workspace_root: &Path, package_name: &str) -> Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", package_name])
        .current_dir(workspace_root)
        .status()
        .context("Failed to run cargo build — is Rust installed?")?;
    if !status.success() {
        bail!("cargo build failed in {}", workspace_root.display());
    }
    Ok(())
}

/// Find the compiled dynamic library in `target/release/` by trying common name patterns.
fn find_built_lib(crate_dir: &Path, plugin_name: &str) -> Result<PathBuf> {
    let safe_name = plugin_name.replace('-', "_");
    let candidates = [
        format!("lib{}.{}", safe_name, dylib_ext()),
        format!("lib{}_{}.{}", "rlean_plugin", safe_name, dylib_ext()),
    ];
    let release_dir = crate_dir.join("target").join("release");
    for c in &candidates {
        let p = release_dir.join(c);
        if p.exists() {
            return Ok(p);
        }
    }
    bail!(
        "Could not find compiled plugin library in {}.\n\
         Expected one of: {}",
        release_dir.display(),
        candidates.join(", ")
    )
}

// ── Misc ──────────────────────────────────────────────────────────────────────

fn now_utc() -> String {
    // RFC 3339 timestamp without external crates — use chrono which is already a dep.
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max { s } else { &s[..max] }
}
