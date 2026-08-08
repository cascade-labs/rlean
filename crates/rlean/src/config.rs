/// ~/.rlean/config management
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Native providers that may store credentials under `providers` in ~/.rlean/config.
pub const KNOWN_PROVIDERS: &[&str] = &["thetadata", "massive", "tradier", "fred"];

// ── Paths ─────────────────────────────────────────────────────────────────────

pub fn rlean_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".rlean"))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("config"))
}

fn legacy_integration_configs_path() -> Result<PathBuf> {
    Ok(rlean_dir()?.join("integration-configs.json"))
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

    /// Native provider credentials: `providers.<name>.<key>`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, BTreeMap<String, String>>,

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
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            serde_json::from_str(&text)
                .with_context(|| format!("Failed to parse {}", path.display()))?
        } else {
            Self::default()
        };
        if cfg.migrate_legacy_integration_configs()? {
            cfg.save()?;
        }
        Ok(cfg)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let text = serde_json::to_string_pretty(self)?;
        atomic_write(&path, &text)?;
        secure_owner_read_write(&path)
    }

    /// Return a provider's stored settings (empty when not configured).
    pub fn get_provider(&self, provider: &str) -> BTreeMap<String, String> {
        self.providers.get(provider).cloned().unwrap_or_default()
    }

    /// Insert or overwrite a key in the given provider's config section.
    pub fn set_provider_key(&mut self, provider: &str, key: &str, value: String) -> Result<()> {
        ensure_known_provider(provider)?;
        self.providers
            .entry(provider.to_string())
            .or_default()
            .insert(key.to_string(), value);
        Ok(())
    }

    /// One-time copy of allowlisted keys from the legacy integrations file.
    ///
    /// Returns true when the config was mutated and should be saved. The legacy
    /// file is renamed to `integration-configs.json.bak` so secrets are kept.
    fn migrate_legacy_integration_configs(&mut self) -> Result<bool> {
        if !self.providers.is_empty() {
            return Ok(false);
        }
        let legacy_path = legacy_integration_configs_path()?;
        if !legacy_path.exists() {
            return Ok(false);
        }
        let text = std::fs::read_to_string(&legacy_path)
            .with_context(|| format!("Failed to read {}", legacy_path.display()))?;
        let legacy: BTreeMap<String, serde_json::Map<String, serde_json::Value>> =
            serde_json::from_str(&text)
                .with_context(|| format!("Failed to parse {}", legacy_path.display()))?;

        let mut migrated = false;
        for &provider in KNOWN_PROVIDERS {
            let Some(section) = legacy.get(provider) else {
                continue;
            };
            let mut entries = BTreeMap::new();
            for (key, value) in section {
                let Some(string) = json_value_as_config_string(value) else {
                    continue;
                };
                entries.insert(key.clone(), string);
            }
            if !entries.is_empty() {
                self.providers.insert(provider.to_string(), entries);
                migrated = true;
            }
        }

        let bak = legacy_path.with_extension("json.bak");
        std::fs::rename(&legacy_path, &bak).with_context(|| {
            format!(
                "Failed to rename {} → {}",
                legacy_path.display(),
                bak.display()
            )
        })?;
        // Rename even when no allowlisted keys were present so load stops
        // seeing the legacy file. `migrated` only affects whether providers
        // changed; either way the rename is a durable side effect and we
        // persist the (possibly still empty) providers map.
        let _ = migrated;
        Ok(true)
    }
}

pub fn ensure_known_provider(provider: &str) -> Result<()> {
    if KNOWN_PROVIDERS.contains(&provider) {
        return Ok(());
    }
    bail!(
        "Unknown provider '{provider}'. Known providers: {}",
        KNOWN_PROVIDERS.join(", ")
    )
}

fn json_value_as_config_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_provider() {
        assert!(ensure_known_provider("thetadata").is_ok());
        assert!(ensure_known_provider("massive").is_ok());
        assert!(ensure_known_provider("tradier").is_ok());
        assert!(ensure_known_provider("fred").is_ok());
        assert!(ensure_known_provider("fidelity").is_err());
    }

    #[test]
    fn set_provider_key_writes_allowlisted_section() {
        let mut cfg = GlobalConfig::default();
        cfg.set_provider_key("thetadata", "api_key", "td-key".into())
            .unwrap();
        cfg.set_provider_key("thetadata", "max_concurrent", "4".into())
            .unwrap();
        assert!(cfg
            .set_provider_key("fidelity", "username", "x".into())
            .is_err());
        assert_eq!(
            cfg.get_provider("thetadata")
                .get("api_key")
                .map(String::as_str),
            Some("td-key")
        );
        assert_eq!(
            cfg.get_provider("thetadata")
                .get("max_concurrent")
                .map(String::as_str),
            Some("4")
        );
    }

    #[test]
    fn json_value_as_config_string_coerces_scalars() {
        assert_eq!(
            json_value_as_config_string(&serde_json::json!("  key  ")).as_deref(),
            Some("key")
        );
        assert_eq!(
            json_value_as_config_string(&serde_json::json!(4)).as_deref(),
            Some("4")
        );
        assert_eq!(
            json_value_as_config_string(&serde_json::json!(true)).as_deref(),
            Some("true")
        );
        assert_eq!(json_value_as_config_string(&serde_json::json!({})), None);
    }
}
