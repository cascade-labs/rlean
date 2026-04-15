/// `rlean plugin` — plugin registry management
///
/// Plugins are `cdylib` crates that export `rlean_plugin_descriptor()`.
/// Installed plugins live in ~/.rlean/plugins/ as .dylib/.so files.
/// The manifest at ~/.rlean/plugins.json tracks what is installed.
///
/// Registries are stored in ~/.rlean/registries.json.
/// The official registry is always included automatically.
///
/// Usage:
///   rlean plugin list                               # list plugins from all registries
///   rlean plugin list --installed                   # show installed plugins
///   rlean plugin install <name>                     # install from any registry
///   rlean plugin install <git-url>                  # install ad-hoc plugin from git URL
///   rlean plugin upgrade <name>                     # rebuild from updated source
///   rlean plugin remove <name>                      # remove installed plugin
///   rlean plugin registry list                      # show configured registries
///   rlean plugin registry add <url>                 # add a custom registry
///   rlean plugin registry remove <url>              # remove a custom registry
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Raw GitHub URL for the official plugin registry.
const OFFICIAL_REGISTRY_URL: &str =
    "https://raw.githubusercontent.com/cascade-labs/rlean-plugins/main/registry.json";

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct PluginArgs {
    #[command(subcommand)]
    pub command: PluginCommand,
}

#[derive(clap::Subcommand)]
pub enum PluginCommand {
    /// List available plugins from all configured registries
    List {
        /// Show only locally installed plugins
        #[arg(long)]
        installed: bool,
    },
    /// Clone, build, and install a plugin
    Install {
        /// Plugin name (from any registry) or a full git URL for an ad-hoc plugin
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
    /// Manage plugin registries
    Registry {
        #[command(subcommand)]
        command: RegistryCommand,
    },
}

#[derive(clap::Subcommand)]
pub enum RegistryCommand {
    /// List all configured registries
    List,
    /// Add a custom registry URL
    Add {
        /// Registry URL (must serve a registry.json compatible with the rlean format)
        url: String,
    },
    /// Remove a custom registry URL
    Remove {
        /// Registry URL to remove
        url: String,
    },
}

// ── Registry types ────────────────────────────────────────────────────────────

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

#[derive(Debug, Deserialize)]
struct RemoteRegistry {
    #[allow(dead_code)]
    version: String,
    plugins: Vec<RegistryEntry>,
}

/// Persisted list of user-configured registry URLs.
/// The official registry is always fetched in addition to these.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UserRegistries {
    urls: Vec<String>,
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

// ── Registry fetch ────────────────────────────────────────────────────────────

/// Fetch the plugin list from a single registry URL via curl.
fn fetch_registry(url: &str) -> Result<Vec<RegistryEntry>> {
    let output = Command::new("curl")
        .args(["--silent", "--fail", "--location", "--max-time", "10", url])
        .output()
        .context("Failed to run curl — is it installed?")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to fetch registry from {url}: {stderr}");
    }

    let body = String::from_utf8(output.stdout)
        .context("Registry response is not valid UTF-8")?;

    // Support both wrapped {"version":…,"plugins":[…]} and bare […] formats.
    if body.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<RegistryEntry>>(&body)
            .context("Failed to parse registry JSON array")
    } else {
        let r: RemoteRegistry = serde_json::from_str(&body)
            .context("Failed to parse registry JSON")?;
        Ok(r.plugins)
    }
}

/// Fetch plugins from all configured registries (official + user-added).
/// Entries from later registries that share a name with an earlier one are skipped
/// so the official registry takes precedence for name collisions.
fn fetch_all_registries() -> Vec<RegistryEntry> {
    let mut urls = vec![OFFICIAL_REGISTRY_URL.to_string()];
    if let Ok(user) = load_user_registries() {
        urls.extend(user.urls);
    }

    let mut seen = std::collections::HashSet::new();
    let mut all = vec![];
    for url in &urls {
        match fetch_registry(url) {
            Ok(entries) => {
                for entry in entries {
                    if seen.insert(entry.name.clone()) {
                        all.push(entry);
                    }
                }
            }
            Err(e) => eprintln!("Warning: could not fetch registry {url}: {e}"),
        }
    }
    all
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_plugin(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List { installed } => {
            if installed { cmd_list_installed() } else { cmd_list_registry() }
        }
        PluginCommand::Install { name }  => cmd_install(&name),
        PluginCommand::Upgrade { name }  => cmd_upgrade(&name),
        PluginCommand::Remove  { name }  => cmd_remove(&name),
        PluginCommand::Registry { command } => match command {
            RegistryCommand::List         => cmd_registry_list(),
            RegistryCommand::Add { url }  => cmd_registry_add(&url),
            RegistryCommand::Remove { url } => cmd_registry_remove(&url),
        },
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_list_registry() -> Result<()> {
    let plugins = fetch_all_registries();

    let installed = load_manifest()?.plugins;
    let installed_names: std::collections::HashSet<&str> =
        installed.iter().map(|e| e.info.name.as_str()).collect();

    if plugins.is_empty() && installed.is_empty() {
        println!("No plugins available. Check your network or run `rlean plugin registry list`.");
        return Ok(());
    }

    println!("{:<22} {:<16} {:<55} {}", "NAME", "KIND", "DESCRIPTION", "STATUS");
    println!("{}", "-".repeat(100));
    for entry in &plugins {
        let status = if installed_names.contains(entry.name.as_str()) { "installed" } else { "" };
        println!(
            "{:<22} {:<16} {:<55} {}",
            entry.name,
            entry.kind,
            entry.description,
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
    println!("{:<22} {:<10} {:<28} {}", "NAME", "VERSION", "KIND", "INSTALLED");
    println!("{}", "-".repeat(78));
    for entry in &manifest.plugins {
        println!(
            "{:<22} {:<10} {:<28} {}",
            entry.info.name, entry.info.version, entry.info.kind, entry.installed_at
        );
    }
    Ok(())
}

fn cmd_install(name: &str) -> Result<()> {
    let entry = resolve_entry(name)?;

    let mut manifest = load_manifest()?;
    if manifest.plugins.iter().any(|e| e.info.name == entry.name) {
        bail!("Plugin '{}' is already installed. Use `rlean plugin upgrade {}` to update.", name, name);
    }

    let src_dir  = plugin_src_dir(&entry.name)?;
    let lib_path = plugin_lib_path(&entry.name)?;

    println!("Cloning {} ...", entry.git_url);
    git_clone(&entry.git_url, &src_dir)?;

    println!("Resolving dependencies for '{}' ...", entry.name);
    cargo_update(&src_dir)?;

    println!("Building {} ...", entry.name);
    let package_name = package_name_for(&entry.name);
    cargo_build(&src_dir, &package_name)?;

    let built = find_built_lib(&src_dir, &entry.name)?;
    std::fs::copy(&built, &lib_path)
        .with_context(|| format!("Failed to copy {} → {}", built.display(), lib_path.display()))?;
    adhoc_codesign(&lib_path);

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

    let src_dir  = PathBuf::from(&manifest.plugins[idx].src_path);
    let lib_path = PathBuf::from(&manifest.plugins[idx].lib_path);

    println!("Pulling latest source for '{}' ...", name);
    git_pull(&src_dir)?;

    println!("Updating dependencies for '{}' ...", name);
    cargo_update(&src_dir)?;

    println!("Building '{}' ...", name);
    let package_name = package_name_for(name);
    cargo_build(&src_dir, &package_name)?;

    let built = find_built_lib(&src_dir, name)?;
    std::fs::copy(&built, &lib_path)
        .with_context(|| format!("Failed to copy {} → {}", built.display(), lib_path.display()))?;
    adhoc_codesign(&lib_path);

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

// ── Registry lookup ───────────────────────────────────────────────────────────

/// Resolve a name or git URL to a RegistryEntry.
/// - Full git URL → synthesise an entry (ad-hoc install, no registry needed)
/// - Short name   → search all configured registries
fn resolve_entry(name_or_url: &str) -> Result<RegistryEntry> {
    if name_or_url.starts_with("http") {
        let inferred_name = name_or_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(name_or_url)
            .trim_start_matches("rlean-plugin-")
            .to_string();
        return Ok(RegistryEntry {
            name:        inferred_name,
            version:     "unknown".to_string(),
            kind:        "unknown".to_string(),
            description: String::new(),
            git_url:     name_or_url.to_string(),
            subdir:      None,
        });
    }

    fetch_all_registries()
        .into_iter()
        .find(|e| e.name == name_or_url)
        .ok_or_else(|| anyhow::anyhow!(
            "Plugin '{}' not found in any registry.\n\
             Run `rlean plugin list` to see available plugins, or pass a git URL directly.",
            name_or_url
        ))
}

// ── Registry management commands ──────────────────────────────────────────────

fn cmd_registry_list() -> Result<()> {
    let user = load_user_registries()?;
    println!("{:<6} {}", "TYPE", "URL");
    println!("{}", "-".repeat(72));
    println!("{:<6} {}", "built-in", OFFICIAL_REGISTRY_URL);
    for url in &user.urls {
        println!("{:<6} {}", "user", url);
    }
    Ok(())
}

fn cmd_registry_add(url: &str) -> Result<()> {
    let mut user = load_user_registries()?;
    if user.urls.iter().any(|u| u == url) {
        bail!("Registry '{}' is already configured.", url);
    }
    user.urls.push(url.to_string());
    save_user_registries(&user)?;
    println!("Added registry: {url}");
    Ok(())
}

fn cmd_registry_remove(url: &str) -> Result<()> {
    let mut user = load_user_registries()?;
    let before = user.urls.len();
    user.urls.retain(|u| u != url);
    if user.urls.len() == before {
        bail!("Registry '{}' not found.", url);
    }
    save_user_registries(&user)?;
    println!("Removed registry: {url}");
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
    let lib_name = format!("librlean_plugin_{}.{}", name.replace('-', "_"), dylib_ext());
    Ok(plugins_dir()?.join(lib_name))
}

fn manifest_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".rlean").join("plugins.json"))
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

fn package_name_for(plugin_name: &str) -> String {
    format!("rlean-plugin-{}", plugin_name)
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
    let d = dir.display().to_string();
    // Discard build-generated changes (e.g. Cargo.lock) before pulling.
    Command::new("git")
        .args(["-C", &d, "reset", "--hard", "HEAD"])
        .status()
        .context("Failed to run git reset")?;
    let status = Command::new("git")
        .args(["-C", &d, "pull", "--ff-only"])
        .status()
        .context("Failed to run git pull")?;
    if !status.success() {
        bail!("git pull failed in {}", dir.display());
    }
    // Delete Cargo.lock so cargo re-resolves git dependencies to their latest HEAD
    // rather than the commit that was pinned at install time.
    let lock = dir.join("Cargo.lock");
    if lock.exists() {
        let _ = std::fs::remove_file(&lock);
    }
    Ok(())
}

fn cargo_update(workspace_root: &Path) -> Result<()> {
    // Force cargo to re-fetch git dependencies (e.g. rlean crates) to their latest HEAD.
    Command::new("cargo")
        .args(["update"])
        .current_dir(workspace_root)
        .status()
        .context("Failed to run cargo update")?;
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

/// Find the compiled dynamic library in `target/release/`.
fn find_built_lib(crate_dir: &Path, plugin_name: &str) -> Result<PathBuf> {
    let safe_name = plugin_name.replace('-', "_");
    let candidates = [
        format!("librlean_plugin_{}.{}", safe_name, dylib_ext()),
        format!("lib{}.{}", safe_name, dylib_ext()),
    ];
    let release_dir = crate_dir.join("target").join("release");
    for c in &candidates {
        let p = release_dir.join(c);
        if p.exists() {
            return Ok(p);
        }
    }
    bail!(
        "Could not find compiled plugin library in {}.\nExpected one of: {}",
        release_dir.display(),
        candidates.join(", ")
    )
}

// ── macOS code signing ────────────────────────────────────────────────────────

fn adhoc_codesign(path: &Path) {
    #[cfg(target_os = "macos")]
    {
        let status = Command::new("codesign")
            .args(["-s", "-", "--force", path.to_str().unwrap_or("")])
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("codesign exited with status {s} for {}", path.display()),
            Err(e) => eprintln!("codesign not available: {e}"),
        }
    }
    #[cfg(not(target_os = "macos"))]
    let _ = path;
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
    std::fs::write(&path, serde_json::to_string_pretty(manifest)?)?;
    Ok(())
}

fn load_user_registries() -> Result<UserRegistries> {
    let path = home_dir()?.join(".rlean").join("registries.json");
    if !path.exists() {
        return Ok(UserRegistries::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).context("Failed to parse registries.json")
}

fn save_user_registries(r: &UserRegistries) -> Result<()> {
    let path = home_dir()?.join(".rlean").join("registries.json");
    std::fs::write(&path, serde_json::to_string_pretty(r)?)?;
    Ok(())
}

// ── Misc ──────────────────────────────────────────────────────────────────────

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

