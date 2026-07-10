//! Resolve the market-data store mode + S3 settings from env vars and the
//! `~/.rlean/config` store.
//!
//! Precedence (highest first):
//! 1. Env vars (`RLEAN_DATA_STORE`, `RLEAN_DATA_S3`, `RLEAN_DATA_S3_*`)
//! 2. `~/.rlean/config` (`data_store`, `data_s3`, `data_s3_*`)
//!
//! There are no CLI flags for the data store yet.
//!
//! Credentials come from the config store: the data-store-specific keys
//! (`data_s3_endpoint` / `_region` / `_access_key` / `_secret_key`) take
//! precedence, falling back to the shared `s3_*` keys (and their `RLEAN_S3_*`
//! env vars) so one credential set can serve both the data store and the
//! artifact relay.
//!
//! The two modes use different Iceberg catalogs:
//! - `local`: plain local-filesystem warehouse + SQLite catalog (no server).
//! - `s3`: S3 warehouse + Lakekeeper REST catalog. `data_catalog`
//!   (`RLEAN_DATA_CATALOG`, default `http://localhost:8181/catalog`) and
//!   `data_warehouse` (`RLEAN_DATA_WAREHOUSE`, default `rlean`) apply only here.
//!
//! Default is local-only. Nothing changes for users who do not opt in.

use anyhow::{bail, Result};
use lean_storage::{RestCatalogConnection, S3DataStoreConfig};

use crate::config::GlobalConfig;

/// Default Iceberg REST catalog (Lakekeeper) URL when `data_catalog` is unset.
const DEFAULT_DATA_CATALOG: &str = "http://localhost:8181/catalog";
/// Default Lakekeeper warehouse name when `data_warehouse` is unset.
const DEFAULT_DATA_WAREHOUSE: &str = "rlean";

/// Which backend the market-data store reads from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataStoreMode {
    Local,
    S3,
}

/// Resolved market-data store configuration for a run.
pub(crate) struct ResolvedDataStoreConfig {
    pub mode: DataStoreMode,
    /// S3 warehouse settings + REST catalog connection. `Some` only in S3 mode.
    pub s3: Option<S3DataStoreConfig>,
    pub catalog: Option<RestCatalogConnection>,
}

/// Read an env var, treating an empty/whitespace value as unset.
fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve the mode, applying env > config precedence. Default is Local.
fn resolve_mode(config: &GlobalConfig) -> Result<DataStoreMode> {
    let raw = env("RLEAN_DATA_STORE").or_else(|| config.data_store.clone());
    match raw {
        None => Ok(DataStoreMode::Local),
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "local" => Ok(DataStoreMode::Local),
            "s3" => Ok(DataStoreMode::S3),
            other => bail!("invalid data_store '{other}', expected local|s3"),
        },
    }
}

/// Resolve the full market-data store configuration for a run.
pub(crate) fn resolve(config: &GlobalConfig) -> Result<ResolvedDataStoreConfig> {
    let mode = resolve_mode(config)?;
    if mode == DataStoreMode::Local {
        return Ok(ResolvedDataStoreConfig {
            mode,
            s3: None,
            catalog: None,
        });
    }

    let warehouse = env("RLEAN_DATA_S3")
        .or_else(|| config.data_s3.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "data store 's3' requires an S3 warehouse; set RLEAN_DATA_S3, or \
                 `rlean config set data_s3 s3://bucket/prefix`"
            )
        })?;

    let endpoint = env("RLEAN_DATA_S3_ENDPOINT")
        .or_else(|| config.data_s3_endpoint.clone())
        .or_else(|| env("RLEAN_S3_ENDPOINT"))
        .or_else(|| config.s3_endpoint.clone())
        .ok_or_else(|| missing("endpoint", "data_s3_endpoint"))?;
    let region = env("RLEAN_DATA_S3_REGION")
        .or_else(|| config.data_s3_region.clone())
        .or_else(|| env("RLEAN_S3_REGION"))
        .or_else(|| config.s3_region.clone())
        .ok_or_else(|| missing("region", "data_s3_region"))?;
    let access_key = env("RLEAN_DATA_S3_ACCESS_KEY")
        .or_else(|| config.data_s3_access_key.clone())
        .or_else(|| env("RLEAN_S3_ACCESS_KEY"))
        .or_else(|| config.s3_access_key.clone())
        .ok_or_else(|| missing("access key", "data_s3_access_key"))?;
    let secret_key = env("RLEAN_DATA_S3_SECRET_KEY")
        .or_else(|| config.data_s3_secret_key.clone())
        .or_else(|| env("RLEAN_S3_SECRET_KEY"))
        .or_else(|| config.s3_secret_key.clone())
        .ok_or_else(|| missing("secret key", "data_s3_secret_key"))?;

    let catalog_uri = env("RLEAN_DATA_CATALOG")
        .or_else(|| config.data_catalog.clone())
        .unwrap_or_else(|| DEFAULT_DATA_CATALOG.to_string());
    let catalog_warehouse = env("RLEAN_DATA_WAREHOUSE")
        .or_else(|| config.data_warehouse.clone())
        .unwrap_or_else(|| DEFAULT_DATA_WAREHOUSE.to_string());

    Ok(ResolvedDataStoreConfig {
        mode,
        s3: Some(S3DataStoreConfig {
            warehouse,
            endpoint,
            region,
            access_key,
            secret_key,
        }),
        catalog: Some(RestCatalogConnection {
            uri: catalog_uri,
            warehouse: catalog_warehouse,
        }),
    })
}

/// Build a clear "missing credential" error naming the config key to set.
fn missing(what: &str, key: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "data store 's3' requires an S3 {what}; set `rlean config set {key} <value>` \
         (or the shared s3_* key)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> GlobalConfig {
        GlobalConfig::default()
    }

    /// The data-store env vars are process-global; a lock serialises the tests
    /// that mutate them so they cannot race.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear_env() {
        for key in [
            "RLEAN_DATA_STORE",
            "RLEAN_DATA_S3",
            "RLEAN_DATA_S3_ENDPOINT",
            "RLEAN_DATA_S3_REGION",
            "RLEAN_DATA_S3_ACCESS_KEY",
            "RLEAN_DATA_S3_SECRET_KEY",
            "RLEAN_S3_ENDPOINT",
            "RLEAN_S3_REGION",
            "RLEAN_S3_ACCESS_KEY",
            "RLEAN_S3_SECRET_KEY",
            "RLEAN_DATA_CATALOG",
            "RLEAN_DATA_WAREHOUSE",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn default_is_local() {
        let _guard = env_lock();
        clear_env();
        let cfg = base_config();
        let resolved = resolve(&cfg).unwrap();
        assert_eq!(resolved.mode, DataStoreMode::Local);
        assert!(resolved.s3.is_none());
    }

    #[test]
    fn s3_requires_warehouse_and_creds() {
        let _guard = env_lock();
        clear_env();
        let mut cfg = base_config();
        cfg.data_store = Some("s3".to_string());
        // No warehouse yet.
        assert!(resolve(&cfg).is_err());

        cfg.data_s3 = Some("s3://rlean_data/iceberg".to_string());
        // Warehouse set but no credentials.
        assert!(resolve(&cfg).is_err());

        cfg.data_s3_endpoint = Some("https://example.com".to_string());
        cfg.data_s3_region = Some("us-ashburn-1".to_string());
        cfg.data_s3_access_key = Some("ak".to_string());
        cfg.data_s3_secret_key = Some("sk".to_string());
        let resolved = resolve(&cfg).unwrap();
        assert_eq!(resolved.mode, DataStoreMode::S3);
        let s3 = resolved.s3.unwrap();
        assert_eq!(s3.warehouse, "s3://rlean_data/iceberg");
        assert_eq!(s3.endpoint, "https://example.com");
        assert_eq!(s3.region, "us-ashburn-1");
        assert_eq!(s3.access_key, "ak");
        assert_eq!(s3.secret_key, "sk");
        // Catalog defaults apply when data_catalog / data_warehouse are unset.
        let catalog = resolved.catalog.unwrap();
        assert_eq!(catalog.uri, DEFAULT_DATA_CATALOG);
        assert_eq!(catalog.warehouse, DEFAULT_DATA_WAREHOUSE);
    }

    #[test]
    fn catalog_overrides_apply_in_s3_mode() {
        let _guard = env_lock();
        clear_env();
        let mut cfg = base_config();
        cfg.data_store = Some("s3".to_string());
        cfg.data_s3 = Some("s3://rlean-data/iceberg-lk".to_string());
        cfg.data_s3_endpoint = Some("https://example.com".to_string());
        cfg.data_s3_region = Some("us-ashburn-1".to_string());
        cfg.data_s3_access_key = Some("ak".to_string());
        cfg.data_s3_secret_key = Some("sk".to_string());
        cfg.data_catalog = Some("http://catalog.example:8181/catalog".to_string());
        cfg.data_warehouse = Some("rlean_lk".to_string());
        let catalog = resolve(&cfg).unwrap().catalog.unwrap();
        assert_eq!(catalog.uri, "http://catalog.example:8181/catalog");
        assert_eq!(catalog.warehouse, "rlean_lk");
    }

    #[test]
    fn unknown_mode_errors() {
        let _guard = env_lock();
        clear_env();
        let mut cfg = base_config();
        cfg.data_store = Some("nope".to_string());
        assert!(resolve(&cfg).is_err());
    }

    #[test]
    fn creds_fall_back_to_shared() {
        let _guard = env_lock();
        clear_env();
        let mut cfg = base_config();
        cfg.data_store = Some("s3".to_string());
        cfg.data_s3 = Some("s3://rlean_data/iceberg".to_string());
        // Only the shared s3_* keys are set; the data store must fall back to them.
        cfg.s3_endpoint = Some("https://shared.example".to_string());
        cfg.s3_region = Some("us-ashburn-1".to_string());
        cfg.s3_access_key = Some("shared-ak".to_string());
        cfg.s3_secret_key = Some("shared-sk".to_string());
        let s3 = resolve(&cfg).unwrap().s3.unwrap();
        assert_eq!(s3.endpoint, "https://shared.example");
        assert_eq!(s3.access_key, "shared-ak");
        assert_eq!(s3.secret_key, "shared-sk");

        // Data-store-specific keys win over the shared ones.
        cfg.data_s3_access_key = Some("data-ak".to_string());
        let s3 = resolve(&cfg).unwrap().s3.unwrap();
        assert_eq!(s3.access_key, "data-ak");
    }
}
