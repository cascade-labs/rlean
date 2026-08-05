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

    /// Verglas gateway used for canonical queries, writes, and catalog access.
    #[serde(
        default,
        rename = "verglas_endpoint",
        skip_serializing_if = "Option::is_none"
    )]
    pub verglas_endpoint: Option<String>,

    /// Bearer token used by the Verglas SDK for every discovered endpoint.
    #[serde(
        default,
        rename = "verglas_token",
        skip_serializing_if = "Option::is_none"
    )]
    pub verglas_token: Option<String>,

    // ── Run artifact relay (backtest/live run dirs → S3) ──────────────────────
    /// Where run artifacts are written: `local` (default), `s3`, or `mirror`.
    #[serde(
        default,
        rename = "artifact_store",
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_store: Option<String>,

    /// Destination for artifact uploads as `s3://bucket/prefix`.
    #[serde(
        default,
        rename = "artifact_s3",
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_s3: Option<String>,

    /// Artifact store endpoint URL.
    #[serde(
        default,
        rename = "artifact_s3_endpoint",
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_s3_endpoint: Option<String>,

    /// Artifact store region.
    #[serde(
        default,
        rename = "artifact_s3_region",
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_s3_region: Option<String>,

    /// Artifact store access key.
    #[serde(
        default,
        rename = "artifact_s3_access_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_s3_access_key: Option<String>,

    /// Artifact store secret key.
    #[serde(
        default,
        rename = "artifact_s3_secret_key",
        skip_serializing_if = "Option::is_none"
    )]
    pub artifact_s3_secret_key: Option<String>,

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
        atomic_write(&path, &text)?;
        secure_owner_read_write(&path)
    }
}

// ── Credentials (~/.rlean/credentials) ────────────────────────────────────────

// ── Workspace config (rlean.json) ─────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkspaceConfig {
    #[serde(default = "default_language")]
    pub default_language: String,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
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

// ── Provider integration configs ───────────────────────────────────────────────

pub fn integration_configs_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("integration-configs.json"))
}

/// Provider-specific credentials used by native integrations.
///
/// The outer map key is the integration name (for example `"thetadata"`).
/// rlean never interprets the inner map.
#[derive(Debug, Serialize, Deserialize, Default)]
pub struct IntegrationConfigs(
    pub std::collections::HashMap<String, serde_json::Map<String, serde_json::Value>>,
);

impl IntegrationConfigs {
    pub fn load() -> Result<Self> {
        let path = integration_configs_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = integration_configs_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(&self.0)?;
        atomic_write(&path, &text)?;
        secure_owner_read_write(&path)
    }

    /// Return an integration's stored config map (empty when not configured).
    pub fn get_integration(&self, integration: &str) -> serde_json::Map<String, serde_json::Value> {
        self.0.get(integration).cloned().unwrap_or_default()
    }

    /// Insert or overwrite a key in the given integration's config section.
    pub fn set_key(&mut self, integration: &str, key: &str, value: serde_json::Value) {
        self.0
            .entry(integration.to_string())
            .or_default()
            .insert(key.to_string(), value);
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Write to a temp file then rename (atomic on same filesystem).
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).with_context(|| format!("Failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("Failed to rename {} → {}", tmp.display(), path.display()))
}

#[cfg(unix)]
pub(crate) fn secure_owner_read_write(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = std::fs::metadata(path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .permissions();
    permissions.set_mode(0o600);
    std::fs::set_permissions(path, permissions)
        .with_context(|| format!("Failed to set secure permissions on {}", path.display()))
}

#[cfg(not(unix))]
pub(crate) fn secure_owner_read_write(_path: &Path) -> Result<()> {
    Ok(())
}
