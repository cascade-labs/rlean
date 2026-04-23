/// ~/.rlean/config and ~/.rlean/credentials management
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ── Paths ─────────────────────────────────────────────────────────────────────

pub fn rlean_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".rlean"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("config"))
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(PathBuf::from))
        .context("Cannot determine home directory (HOME env not set)")
}

// ── Global config (~/.rlean/config) ──────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct GlobalConfig {
    #[serde(default = "default_language")]
    pub default_language: String,

    /// Global Parquet data root directory
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_folder: Option<String>,

    /// Last workspace initialised with `rlean init`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
}

fn default_language() -> String {
    "python".to_string()
}

impl GlobalConfig {
    pub fn load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)
    }
}

// ── Credentials (~/.rlean/credentials) ────────────────────────────────────────

// ── Workspace config (rlean.json) ─────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkspaceConfig {
    #[serde(default = "default_data_folder")]
    pub data_folder: String,

    #[serde(default = "default_language")]
    pub default_language: String,
}

fn default_data_folder() -> String {
    "data".to_string()
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            data_folder: default_data_folder(),
            default_language: default_language(),
        }
    }
}

impl WorkspaceConfig {
    pub fn load(workspace: &Path) -> Result<Self> {
        let path = workspace.join("rlean.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self, workspace: &Path) -> Result<()> {
        let path = workspace.join("rlean.json");
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)
    }
}

// ── Project config (config.json) ──────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProjectConfig {
    pub algorithm_language: String,
    pub parameters: serde_json::Map<String, serde_json::Value>,
    pub description: String,
    pub local_id: u64,
}

impl ProjectConfig {
    pub fn new(language: &str) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        // deterministic-ish 9-digit local id
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
        let local_id = 100_000_000 + (seed % 900_000_000);
        Self {
            algorithm_language: language.to_string(),
            parameters: serde_json::Map::new(),
            description: String::new(),
            local_id,
        }
    }

    pub fn load(project_dir: &Path) -> Result<Self> {
        let path = project_dir.join("config.json");
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self, project_dir: &Path) -> Result<()> {
        let path = project_dir.join("config.json");
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)
    }
}

// ── Plugin configs (~/.rlean/plugin-configs.json) ─────────────────────────────

pub fn plugin_configs_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("plugin-configs.json"))
}

/// Per-plugin config store (~/.rlean/plugin-configs.json).
///
/// The outer map key is the plugin name (e.g. `"thetadata"`).
/// The inner map holds arbitrary key/value pairs defined by that plugin.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct PluginConfigs(
    pub std::collections::HashMap<String, serde_json::Map<String, serde_json::Value>>,
);

impl PluginConfigs {
    pub fn load() -> Result<Self> {
        let path = plugin_configs_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = plugin_configs_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(&self.0)?;
        atomic_write(&path, &text)
    }

    /// Return the stored config map for the given plugin (empty map if not set).
    pub fn get_plugin(&self, plugin: &str) -> serde_json::Map<String, serde_json::Value> {
        self.0.get(plugin).cloned().unwrap_or_default()
    }

    /// Insert or overwrite a key in the given plugin's config section.
    pub fn set_key(&mut self, plugin: &str, key: &str, value: serde_json::Value) {
        self.0
            .entry(plugin.to_string())
            .or_default()
            .insert(key.to_string(), value);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Write to a temp file then rename (atomic on same filesystem).
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} → {}", tmp.display(), path.display()))
}
