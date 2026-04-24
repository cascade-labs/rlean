/// `rlean plugin` — install, upgrade, remove, and list plugins
///
/// Plugins are `cdylib` crates that export `rlean_plugin_descriptor()`.
/// Installed plugins live in ~/.rlean/plugins/ as .dylib/.so files.
/// The manifest at ~/.rlean/plugins.json tracks what is installed.
///
/// Registries are managed separately with `rlean registry`.
///
/// Usage:
///   rlean plugin list                               # list plugins from all registries
///   rlean plugin list --installed                   # show installed plugins
///   rlean plugin install <name>[,<name>...]         # install from any registry
///   rlean plugin install <git-url>                  # install ad-hoc plugin from git URL
///   rlean plugin upgrade <name>                     # rebuild from updated source
///   rlean plugin remove <name>                      # remove installed plugin
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

type PackagePathMap = BTreeMap<String, PathBuf>;
type PatchEntries = Vec<(String, PathBuf)>;

/// Raw GitHub URL for the official plugin registry.
pub(crate) const OFFICIAL_REGISTRY_URL: &str =
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
    /// Clone, build, and install one or more plugins
    Install {
        /// Plugin name(s), comma-separated, or a full git URL for an ad-hoc plugin
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

// ── Registry types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub name: String,
    pub version: String,
    pub kind: String,
    pub description: String,
    /// Git URL used to clone + build the plugin
    pub git_url: String,
    /// Optional: subdirectory inside the repo containing the plugin crate
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subdir: Option<String>,
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
pub(crate) struct UserRegistries {
    pub(crate) urls: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct InstalledManifest {
    plugins: Vec<InstalledEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct InstalledEntry {
    #[serde(flatten)]
    pub info: RegistryEntry,
    pub installed_at: String,
    /// Absolute path to the compiled .dylib/.so
    pub lib_path: String,
    /// Checkout path (for upgrade)
    pub src_path: String,
}

struct InstallGroup {
    git_url: String,
    src_dir: PathBuf,
    entries: Vec<RegistryEntry>,
}

// ── Registry fetch ────────────────────────────────────────────────────────────

/// Fetch the plugin list from a single registry URL.
///
/// Supports:
/// - `https://` / `http://` — fetched via curl
/// - `file:///path/to/registry.json` — read directly from disk
/// - `/absolute/path/to/registry.json` — read directly from disk
///
/// Try to get a GitHub token for authenticating requests to private repos.
///
/// Checks (in order): GH_TOKEN env var, GITHUB_TOKEN env var, `gh auth token`.
fn github_token() -> Option<String> {
    if let Ok(t) = std::env::var("GH_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.is_empty() {
            return Some(t);
        }
    }
    // Try the gh CLI
    if let Ok(out) = Command::new("gh").args(["auth", "token"]).output() {
        if out.status.success() {
            let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !t.is_empty() {
                return Some(t);
            }
        }
    }
    None
}

fn fetch_registry(url: &str) -> Result<Vec<RegistryEntry>> {
    let body = if let Some(path) = url.strip_prefix("file://") {
        std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read registry file: {path}"))?
    } else if url.starts_with('/') {
        std::fs::read_to_string(url)
            .with_context(|| format!("Failed to read registry file: {url}"))?
    } else {
        let is_github = url.contains("raw.githubusercontent.com") || url.contains("github.com");
        let mut curl_args = vec!["--silent", "--fail", "--location", "--max-time", "10"];

        // Collect token into a local binding so it lives long enough
        let token_str;
        if is_github {
            if let Some(token) = github_token() {
                token_str = format!("Authorization: Bearer {token}");
                curl_args.extend(["-H", token_str.as_str()]);
            }
        }

        let output = Command::new("curl")
            .args(&curl_args)
            .arg(url)
            .output()
            .context("Failed to run curl — is it installed?")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("Failed to fetch registry from {url}: {stderr}");
        }

        String::from_utf8(output.stdout).context("Registry response is not valid UTF-8")?
    };

    // Support both wrapped {"version":…,"plugins":[…]} and bare […] formats.
    if body.trim_start().starts_with('[') {
        serde_json::from_str::<Vec<RegistryEntry>>(&body)
            .context("Failed to parse registry JSON array")
    } else {
        let r: RemoteRegistry =
            serde_json::from_str(&body).context("Failed to parse registry JSON")?;
        Ok(r.plugins)
    }
}

/// Fetch plugins from all configured registries (official + user-added).
/// Returns `(registry_label, entry)` pairs.
/// Entries from later registries that share a name with an earlier one are skipped
/// so the official registry takes precedence for name collisions.
fn fetch_all_registries() -> Vec<(String, RegistryEntry)> {
    let mut sources: Vec<(String, String)> =
        vec![("built-in".to_string(), OFFICIAL_REGISTRY_URL.to_string())];
    if let Ok(user) = load_user_registries() {
        for url in user.urls {
            let label = registry_label(&url);
            sources.push((label, url));
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut all = vec![];
    for (label, url) in &sources {
        match fetch_registry(url) {
            Ok(entries) => {
                for entry in entries {
                    if seen.insert(entry.name.clone()) {
                        all.push((label.clone(), entry));
                    }
                }
            }
            Err(e) => eprintln!("Warning: could not fetch registry {url}: {e}"),
        }
    }
    all
}

/// Derive a short human-readable label from a registry URL.
fn registry_label(url: &str) -> String {
    // Strip raw.githubusercontent.com prefix to just "owner/repo"
    if let Some(rest) = url.strip_prefix("https://raw.githubusercontent.com/") {
        let parts: Vec<&str> = rest.splitn(4, '/').collect();
        if parts.len() >= 2 {
            return format!("{}/{}", parts[0], parts[1]);
        }
    }
    // For file:// or absolute paths use just the filename
    if let Some(rest) = url.strip_prefix("file://").or_else(|| {
        if url.starts_with('/') {
            Some(url)
        } else {
            None
        }
    }) {
        return std::path::Path::new(rest)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(url)
            .to_string();
    }
    url.to_string()
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_plugin(args: PluginArgs) -> Result<()> {
    match args.command {
        PluginCommand::List { installed } => {
            if installed {
                cmd_list_installed()
            } else {
                cmd_list_registry()
            }
        }
        PluginCommand::Install { name } => cmd_install(&name),
        PluginCommand::Upgrade { name } => cmd_upgrade(&name),
        PluginCommand::Remove { name } => cmd_remove(&name),
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

    const NAME_W: usize = 22;
    const KIND_W: usize = 16;
    const DESC_W: usize = 50;
    const REG_W: usize = 24;

    println!(
        "{:<NAME_W$} {:<KIND_W$} {:<DESC_W$} {:<REG_W$} STATUS",
        "NAME", "KIND", "DESCRIPTION", "REGISTRY"
    );
    println!(
        "{}",
        "-".repeat(NAME_W + 1 + KIND_W + 1 + DESC_W + 1 + REG_W + 1 + 9)
    );
    for (registry, entry) in &plugins {
        let status = if installed_names.contains(entry.name.as_str()) {
            "installed"
        } else {
            ""
        };
        // Truncate description so STATUS column stays aligned
        let desc = if entry.description.len() > DESC_W {
            format!("{}…", &entry.description[..DESC_W - 1])
        } else {
            entry.description.clone()
        };
        println!(
            "{:<NAME_W$} {:<KIND_W$} {:<DESC_W$} {:<REG_W$} {}",
            entry.name, entry.kind, desc, registry, status
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
    println!("{:<22} {:<10} {:<28} INSTALLED", "NAME", "VERSION", "KIND");
    println!("{}", "-".repeat(78));
    for entry in &manifest.plugins {
        println!(
            "{:<22} {:<10} {:<28} {}",
            entry.info.name, entry.info.version, entry.info.kind, entry.installed_at
        );
    }
    Ok(())
}

fn cmd_install(names: &str) -> Result<()> {
    let entries = resolve_install_entries(names)?;
    let mut manifest = load_manifest()?;

    for entry in &entries {
        if manifest.plugins.iter().any(|e| e.info.name == entry.name) {
            bail!(
                "Plugin '{}' is already installed. Use `rlean plugin upgrade {}` to update.",
                entry.name,
                entry.name
            );
        }
    }

    let groups = install_groups(&entries, &manifest)?;
    for group in groups {
        if group.src_dir.exists() {
            println!("Using existing source at {} ...", group.src_dir.display());
        } else {
            println!("Cloning {} ...", group.git_url);
            git_clone(&group.git_url, &group.src_dir)?;
        }
        write_plugin_cargo_config(&group.src_dir)?;

        let names = group
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        println!("Resolving dependencies for {names} ...");
        cargo_update(&group.src_dir)?;

        let package_names = group
            .entries
            .iter()
            .map(|entry| package_name_for(&entry.name))
            .collect::<Vec<_>>();
        println!("Building {names} ...");
        cargo_build_many(&group.src_dir, &package_names)?;

        for entry in group.entries {
            let lib_path = plugin_lib_path(&entry.name)?;
            let built = find_built_lib(&group.src_dir, &entry.name)?;
            std::fs::copy(&built, &lib_path).with_context(|| {
                format!(
                    "Failed to copy {} → {}",
                    built.display(),
                    lib_path.display()
                )
            })?;
            adhoc_codesign(&lib_path);

            manifest.plugins.push(InstalledEntry {
                info: entry.clone(),
                installed_at: now_utc(),
                lib_path: lib_path.display().to_string(),
                src_path: group.src_dir.display().to_string(),
            });

            println!("Installed '{}'  →  {}", entry.name, lib_path.display());
        }
    }
    save_manifest(&manifest)?;

    Ok(())
}

fn cmd_upgrade(name: &str) -> Result<()> {
    let mut manifest = load_manifest()?;
    let idx = manifest
        .plugins
        .iter()
        .position(|e| e.info.name == name)
        .ok_or_else(|| anyhow::anyhow!("Plugin '{}' is not installed.", name))?;

    let src_dir = PathBuf::from(&manifest.plugins[idx].src_path);
    let lib_path = PathBuf::from(&manifest.plugins[idx].lib_path);

    println!("Pulling latest source for '{}' ...", name);
    git_pull(&src_dir)?;
    write_plugin_cargo_config(&src_dir)?;

    println!("Updating dependencies for '{}' ...", name);
    cargo_update(&src_dir)?;

    println!("Building '{}' ...", name);
    let package_name = package_name_for(name);
    cargo_build(&src_dir, &package_name)?;

    let built = find_built_lib(&src_dir, name)?;
    std::fs::copy(&built, &lib_path).with_context(|| {
        format!(
            "Failed to copy {} → {}",
            built.display(),
            lib_path.display()
        )
    })?;
    adhoc_codesign(&lib_path);

    manifest.plugins[idx].installed_at = now_utc();
    save_manifest(&manifest)?;

    println!("Upgraded '{}'.", name);
    Ok(())
}

fn cmd_remove(name: &str) -> Result<()> {
    let mut manifest = load_manifest()?;
    let idx = manifest
        .plugins
        .iter()
        .position(|e| e.info.name == name)
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

/// Returns true if the argument looks like a git URL or local path that
/// can be passed directly to `git clone` rather than resolved via a registry.
fn is_git_url_or_path(s: &str) -> bool {
    s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("ssh://")
        || s.starts_with("git@")
        || s.starts_with("git://")
        || s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
}

/// Resolve a name or git URL to a RegistryEntry.
/// - Git URL / local path → synthesise an entry (ad-hoc install, no registry needed)
/// - Short name           → search all configured registries
fn resolve_entry(name_or_url: &str) -> Result<RegistryEntry> {
    if is_git_url_or_path(name_or_url) {
        let inferred_name = name_or_url
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(name_or_url)
            .trim_start_matches("rlean-plugin-")
            .to_string();
        return Ok(RegistryEntry {
            name: inferred_name,
            version: "unknown".to_string(),
            kind: "unknown".to_string(),
            description: String::new(),
            git_url: name_or_url.to_string(),
            subdir: None,
        });
    }

    fetch_all_registries()
        .into_iter()
        .find(|(_reg, e)| e.name == name_or_url)
        .map(|(_reg, e)| e)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Plugin '{}' not found in any registry.\n\
             Run `rlean plugin list` to see available plugins, or pass a git URL directly.",
                name_or_url
            )
        })
}

fn resolve_install_entries(names_or_url: &str) -> Result<Vec<RegistryEntry>> {
    if is_git_url_or_path(names_or_url) {
        return Ok(vec![resolve_entry(names_or_url)?]);
    }

    let mut seen = BTreeSet::new();
    let mut entries = Vec::new();
    for raw_name in names_or_url.split(',') {
        let name = raw_name.trim();
        if name.is_empty() {
            continue;
        }
        if !seen.insert(name.to_string()) {
            bail!("Plugin '{}' was requested more than once.", name);
        }
        entries.push(resolve_entry(name)?);
    }

    if entries.is_empty() {
        bail!("No plugin names provided.");
    }
    Ok(entries)
}

fn install_groups(
    entries: &[RegistryEntry],
    manifest: &InstalledManifest,
) -> Result<Vec<InstallGroup>> {
    let mut by_git_url: BTreeMap<String, Vec<RegistryEntry>> = BTreeMap::new();
    for entry in entries {
        by_git_url
            .entry(entry.git_url.clone())
            .or_default()
            .push(entry.clone());
    }

    let mut groups = Vec::new();
    for (git_url, entries) in by_git_url {
        let src_dir = source_dir_for_install_group(&git_url, &entries, manifest)?;
        groups.push(InstallGroup {
            git_url,
            src_dir,
            entries,
        });
    }
    Ok(groups)
}

fn source_dir_for_install_group(
    git_url: &str,
    entries: &[RegistryEntry],
    manifest: &InstalledManifest,
) -> Result<PathBuf> {
    if let Some(installed) = manifest
        .plugins
        .iter()
        .find(|installed| installed.info.git_url == git_url)
    {
        return Ok(PathBuf::from(&installed.src_path));
    }

    for entry in entries {
        let dir = plugin_src_dir(&entry.name)?;
        if dir.exists() {
            return Ok(dir);
        }
    }

    plugin_src_dir(&entries[0].name)
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

pub(crate) fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(PathBuf::from))
        .context("HOME env not set")
}

fn dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

fn package_name_for(plugin_name: &str) -> String {
    format!("rlean-plugin-{}", plugin_name.replace('_', "-"))
}

// ── Git + Cargo helpers ───────────────────────────────────────────────────────

fn git_clone(url: &str, dest: &Path) -> Result<()> {
    let status = Command::new("git")
        .args(["clone", "--depth=1", url, &dest.display().to_string()])
        .status()
        .context("Failed to run git clone — is git installed?")?;
    if !status.success() {
        bail!("git clone failed for {}", url);
    }
    Ok(())
}

fn write_plugin_cargo_config(plugin_src: &Path) -> Result<()> {
    let rlean_root = rlean_workspace_root()?;
    let packages = rlean_workspace_packages(&rlean_root)?;
    let (sources, patch_entries) = plugin_rlean_dependency_patches(plugin_src, &packages)?;
    let cargo_dir = plugin_src.join(".cargo");
    std::fs::create_dir_all(&cargo_dir)
        .with_context(|| format!("Failed to create {}", cargo_dir.display()))?;

    let mut config = String::from(
        "# Generated by rlean. Keeps plugin builds ABI-compatible with this CLI.\n\
         [net]\n\
         git-fetch-with-cli = true\n\n",
    );
    for source in sources {
        config.push_str(&format!("[patch.\"{source}\"]\n"));
        for (package_name, path) in &patch_entries {
            config.push_str(&format!(
                "{package_name} = {{ path = \"{}\" }}\n",
                path.display()
            ));
        }
        config.push('\n');
    }

    let config_path = cargo_dir.join("config.toml");
    std::fs::write(&config_path, config)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    Ok(())
}

fn rlean_workspace_packages(root: &Path) -> Result<PackagePathMap> {
    let root_manifest = root.join("Cargo.toml");
    let manifest = std::fs::read_to_string(&root_manifest)
        .with_context(|| format!("Failed to read {}", root_manifest.display()))?;
    let members = parse_workspace_members(&manifest).with_context(|| {
        format!(
            "Failed to parse workspace members from {}",
            root_manifest.display()
        )
    })?;

    let mut packages = BTreeMap::new();
    for member in members {
        let member_path = root.join(&member);
        let member_manifest = member_path.join("Cargo.toml");
        if !member_manifest.exists() {
            continue;
        }
        let manifest = std::fs::read_to_string(&member_manifest)
            .with_context(|| format!("Failed to read {}", member_manifest.display()))?;
        let Some(package_name) = parse_package_name(&manifest) else {
            continue;
        };
        packages.insert(package_name, member_path);
    }

    Ok(packages)
}

fn plugin_rlean_dependency_patches(
    plugin_src: &Path,
    packages: &PackagePathMap,
) -> Result<(BTreeSet<String>, PatchEntries)> {
    let mut manifests = Vec::new();
    collect_cargo_manifests(plugin_src, &mut manifests)?;

    let mut sources = BTreeSet::new();
    let mut package_names = BTreeSet::new();
    for manifest_path in manifests {
        let manifest = std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("Failed to read {}", manifest_path.display()))?;
        for raw_line in manifest.lines() {
            let line = raw_line.split('#').next().unwrap_or("").trim();
            if !line.contains("git") || !line.contains("cascade-labs/rlean") {
                continue;
            }

            let Some((dependency_name, _)) = line.split_once('=') else {
                continue;
            };
            let Some(source) = extract_toml_string_value(line, "git") else {
                continue;
            };

            sources.insert(source);
            let package_name = extract_toml_string_value(line, "package")
                .unwrap_or_else(|| dependency_name.trim().to_string());
            if packages.contains_key(&package_name) {
                package_names.insert(package_name);
            }
        }
    }

    let patch_entries = package_names
        .into_iter()
        .filter_map(|package_name| {
            packages
                .get(&package_name)
                .map(|path| (package_name, path.clone()))
        })
        .collect();

    Ok((sources, patch_entries))
}

fn collect_cargo_manifests(dir: &Path, manifests: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if matches!(name, ".git" | "target") {
                continue;
            }
            collect_cargo_manifests(&path, manifests)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("Cargo.toml") {
            manifests.push(path);
        }
    }
    Ok(())
}

fn extract_toml_string_value(line: &str, key: &str) -> Option<String> {
    let key_start = line.find(key)?;
    let after_key = line[key_start + key.len()..].trim_start();
    let after_equals = after_key.strip_prefix('=')?.trim_start();
    let value = after_equals.strip_prefix('"')?;
    let end = value.find('"')?;
    Some(value[..end].to_string())
}

fn parse_workspace_members(manifest: &str) -> Result<Vec<String>> {
    let mut in_workspace = false;
    let mut in_members = false;
    let mut members = Vec::new();

    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_workspace = line == "[workspace]";
            if !in_workspace && in_members {
                break;
            }
        }
        if !in_workspace {
            continue;
        }

        if line.starts_with("members") && line.contains('[') {
            in_members = true;
        }
        if in_members {
            for part in line.split('"').skip(1).step_by(2) {
                members.push(part.to_string());
            }
            if line.contains(']') {
                break;
            }
        }
    }

    if members.is_empty() {
        bail!("workspace.members is empty or missing")
    }
    Ok(members)
}

fn parse_package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
            continue;
        }
        if !in_package || !line.starts_with("name") {
            continue;
        }

        let (_, value) = line.split_once('=')?;
        return Some(value.trim().trim_matches('"').to_string());
    }
    None
}

fn rlean_workspace_root() -> Result<PathBuf> {
    if let Ok(path) = std::env::var("RLEAN_SOURCE_DIR") {
        let root = PathBuf::from(path);
        if root.join("Cargo.toml").exists() {
            return root
                .canonicalize()
                .with_context(|| format!("Failed to canonicalize {}", root.display()));
        }
        bail!(
            "RLEAN_SOURCE_DIR points to {}, but Cargo.toml was not found",
            root.display()
        );
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("Could not derive rlean workspace root from CARGO_MANIFEST_DIR")?;

    if root.join("Cargo.toml").exists() && root.join("crates").join("lean-plugin").exists() {
        return root
            .canonicalize()
            .with_context(|| format!("Failed to canonicalize {}", root.display()));
    }

    bail!("Could not find rlean workspace root. Set RLEAN_SOURCE_DIR to a local rlean checkout.")
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
    let status = Command::new("cargo")
        .args(["update"])
        .current_dir(workspace_root)
        .status()
        .context("Failed to run cargo update")?;
    if !status.success() {
        bail!("cargo update failed in {}", workspace_root.display());
    }
    Ok(())
}

fn cargo_build(workspace_root: &Path, package_name: &str) -> Result<()> {
    cargo_build_many(workspace_root, &[package_name.to_string()])
}

fn cargo_build_many(workspace_root: &Path, package_names: &[String]) -> Result<()> {
    let mut args = vec!["build".to_string(), "--release".to_string()];
    for package_name in package_names {
        args.push("-p".to_string());
        args.push(package_name.clone());
    }

    let status = Command::new("cargo")
        .args(&args)
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

pub(crate) fn load_user_registries() -> Result<UserRegistries> {
    let path = home_dir()?.join(".rlean").join("registries.json");
    if !path.exists() {
        return Ok(UserRegistries::default());
    }
    let text = std::fs::read_to_string(&path)?;
    serde_json::from_str(&text).context("Failed to parse registries.json")
}

pub(crate) fn save_user_registries(r: &UserRegistries) -> Result<()> {
    let path = home_dir()?.join(".rlean").join("registries.json");
    std::fs::write(&path, serde_json::to_string_pretty(r)?)?;
    Ok(())
}

// ── Misc ──────────────────────────────────────────────────────────────────────

fn now_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
