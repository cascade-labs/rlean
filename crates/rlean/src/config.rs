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

pub fn credentials_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("credentials"))
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
        serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)
    }
}

// ── Credentials (~/.rlean/credentials) ────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub polygon_api_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thetadata_api_key: Option<String>,
}

impl Credentials {
    pub fn load() -> Result<Self> {
        let path = credentials_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = credentials_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)
    }
}

// ── Workspace config (lean.json) ──────────────────────────────────────────────

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
        let path = workspace.join("lean.json");
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self, workspace: &Path) -> Result<()> {
        let path = workspace.join("lean.json");
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
        serde_json::from_str(&text)
            .with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self, project_dir: &Path) -> Result<()> {
        let path = project_dir.join("config.json");
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Write to a temp file then rename (atomic on same filesystem).
fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)
        .with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} → {}", tmp.display(), path.display()))
}
