//! Resolve the REST Iceberg catalog connection for a run.
//!
//! The market-data store is always a REST Iceberg catalog (AWS S3 Tables in
//! production). There is no local/filesystem data store anymore, so a run
//! cannot start without a catalog URI and warehouse.
//!
//! Precedence (highest first):
//! 1. Env vars (`RLEAN_DATA_CATALOG`, `RLEAN_DATA_WAREHOUSE`,
//!    `RLEAN_DATA_SIGV4_REGION`, `RLEAN_DATA_SIGV4_NAME`, `RLEAN_DATA_NAMESPACE`,
//!    `RLEAN_DATA_S3_ENDPOINT`, `RLEAN_DATA_S3_REGION`,
//!    `RLEAN_DATA_S3_ACCESS_KEY_ID`, `RLEAN_DATA_S3_SECRET_ACCESS_KEY`)
//! 2. `~/.rlean/config` (`data_catalog`, `data_warehouse`, `data_sigv4_region`,
//!    `data_sigv4_name`, `data_namespace`, `data_s3_endpoint`, `data_s3_region`,
//!    `data_s3_access_key_id`, `data_s3_secret_access_key`)
//!
//! CLI flags take precedence over both: `main` copies any provided `--data-*`
//! flag into the loaded [`GlobalConfig`] before calling [`resolve`], so a set
//! CLI value wins over the env var read here, which in turn wins over the config
//! file. See `cli::apply_data_catalog_overrides`.

use anyhow::Result;
use rlean_storage::{
    DataS3Config, RestCatalogConfig, SigV4Config, DEFAULT_DATA_REFRESH_SECS, DEFAULT_NAMESPACE,
};

use crate::config::GlobalConfig;

/// Read an env var, treating an empty/whitespace value as unset.
fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve the REST Iceberg catalog connection for a run.
///
/// Hard-errors when any required catalog or S3 data-plane setting is missing.
/// There is no local store and no implicit AWS endpoint fallback.
pub(crate) fn resolve(config: &GlobalConfig) -> Result<RestCatalogConfig> {
    let uri = env("RLEAN_DATA_CATALOG")
        .or_else(|| config.data_catalog.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no market-data catalog configured. rlean reads all data from a REST Iceberg \
                 catalog; there is no local data store. Set the catalog URI with \
                 `rlean config set data_catalog <REST URI>` (or the RLEAN_DATA_CATALOG env var / \
                 --data-catalog flag)."
            )
        })?;

    let warehouse = env("RLEAN_DATA_WAREHOUSE")
        .or_else(|| config.data_warehouse.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no market-data warehouse configured. S3 Tables needs the table-bucket ARN; set \
                 it with `rlean config set data_warehouse <ARN>` (or the RLEAN_DATA_WAREHOUSE env \
                 var / --data-warehouse flag)."
            )
        })?;

    let region = env("RLEAN_DATA_SIGV4_REGION").or_else(|| config.data_sigv4_region.clone());
    let sigv4 = region.map(|region| {
        let signing_name = env("RLEAN_DATA_SIGV4_NAME")
            .or_else(|| config.data_sigv4_name.clone())
            .unwrap_or_else(|| "s3tables".to_string());
        SigV4Config {
            region,
            signing_name,
        }
    });

    let namespace = env("RLEAN_DATA_NAMESPACE")
        .or_else(|| config.data_namespace.clone())
        .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());

    let data_refresh_secs = env("RLEAN_DATA_REFRESH_SECS")
        .and_then(|value| value.parse::<u64>().ok())
        .or(config.data_refresh_secs)
        .unwrap_or(DEFAULT_DATA_REFRESH_SECS);

    let data_s3 = resolve_data_s3(config)?;

    Ok(RestCatalogConfig {
        uri,
        warehouse,
        sigv4,
        namespace,
        data_refresh_secs,
        data_s3,
    })
}

/// Resolve the required S3-compatible data endpoint and its credentials. These
/// settings govern Iceberg metadata, manifest, and Parquet I/O; they are not
/// inferred from the catalog region or bucket name.
fn resolve_data_s3(config: &GlobalConfig) -> Result<DataS3Config> {
    let required = |env_name: &str, configured: Option<String>, config_key: &str| {
        env(env_name).or(configured).ok_or_else(|| {
            anyhow::anyhow!(
                "no Iceberg data S3 configuration for {config_key}. Set it with \
                 `rlean config set {config_key} <value>` (or {env_name})."
            )
        })
    };
    let endpoint = required(
        "RLEAN_DATA_S3_ENDPOINT",
        config.data_s3_endpoint.clone(),
        "data_s3_endpoint",
    )?;
    let region = required(
        "RLEAN_DATA_S3_REGION",
        config.data_s3_region.clone(),
        "data_s3_region",
    )?;
    let access_key_id = required(
        "RLEAN_DATA_S3_ACCESS_KEY_ID",
        config.data_s3_access_key_id.clone(),
        "data_s3_access_key_id",
    )?;
    let secret_access_key = required(
        "RLEAN_DATA_S3_SECRET_ACCESS_KEY",
        config.data_s3_secret_access_key.clone(),
        "data_s3_secret_access_key",
    )?;
    Ok(DataS3Config {
        endpoint,
        region,
        access_key_id,
        secret_access_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog env vars are process-global; a lock serialises the tests that
    /// mutate them so they cannot race.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn clear_env() {
        for key in [
            "RLEAN_DATA_CATALOG",
            "RLEAN_DATA_WAREHOUSE",
            "RLEAN_DATA_SIGV4_REGION",
            "RLEAN_DATA_SIGV4_NAME",
            "RLEAN_DATA_NAMESPACE",
            "RLEAN_DATA_REFRESH_SECS",
            "RLEAN_DATA_S3_ENDPOINT",
            "RLEAN_DATA_S3_REGION",
            "RLEAN_DATA_S3_ACCESS_KEY_ID",
            "RLEAN_DATA_S3_SECRET_ACCESS_KEY",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn missing_catalog_errors() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig::default();
        assert!(resolve(&cfg).is_err());
    }

    /// A complete data-store configuration with the usual optional catalog
    /// settings left at their defaults.
    fn config_with_catalog(uri: &str, warehouse: &str) -> GlobalConfig {
        GlobalConfig {
            data_catalog: Some(uri.to_string()),
            data_warehouse: Some(warehouse.to_string()),
            data_s3_endpoint: Some("http://127.0.0.1:8333".to_string()),
            data_s3_region: Some("us-east-1".to_string()),
            data_s3_access_key_id: Some("TEST_KEY".to_string()),
            data_s3_secret_access_key: Some("TEST_SECRET".to_string()),
            ..GlobalConfig::default()
        }
    }

    #[test]
    fn missing_warehouse_errors() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_catalog: Some("https://s3tables.us-west-2.amazonaws.com/iceberg".to_string()),
            ..GlobalConfig::default()
        };
        assert!(resolve(&cfg).is_err());
    }

    #[test]
    fn config_provides_catalog_and_warehouse_without_sigv4() {
        let _guard = env_lock();
        clear_env();
        let cfg = config_with_catalog("http://localhost:8181/catalog", "rlean");
        let resolved = resolve(&cfg).unwrap();
        assert_eq!(resolved.uri, "http://localhost:8181/catalog");
        assert_eq!(resolved.warehouse, "rlean");
        assert!(resolved.sigv4.is_none());
        assert_eq!(resolved.namespace, "lean");
    }

    #[test]
    fn env_takes_precedence_over_config() {
        let _guard = env_lock();
        clear_env();
        let cfg = config_with_catalog("http://config-uri/catalog", "config-wh");
        std::env::set_var("RLEAN_DATA_CATALOG", "http://env-uri/catalog");
        std::env::set_var("RLEAN_DATA_WAREHOUSE", "env-wh");
        let resolved = resolve(&cfg).unwrap();
        clear_env();
        assert_eq!(resolved.uri, "http://env-uri/catalog");
        assert_eq!(resolved.warehouse, "env-wh");
    }

    #[test]
    fn sigv4_present_defaults_name_to_s3tables() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_sigv4_region: Some("us-west-2".to_string()),
            ..config_with_catalog(
                "https://s3tables.us-west-2.amazonaws.com/iceberg",
                "arn:aws:s3tables:us-west-2:123456789012:bucket/rlean",
            )
        };
        let resolved = resolve(&cfg).unwrap();
        let sigv4 = resolved.sigv4.expect("sigv4 present");
        assert_eq!(sigv4.region, "us-west-2");
        assert_eq!(sigv4.signing_name, "s3tables");
    }

    #[test]
    fn sigv4_name_override_applies() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_sigv4_region: Some("us-east-1".to_string()),
            data_sigv4_name: Some("glue".to_string()),
            ..config_with_catalog("https://glue.example/iceberg", "wh")
        };
        let sigv4 = resolve(&cfg).unwrap().sigv4.expect("sigv4 present");
        assert_eq!(sigv4.signing_name, "glue");
    }

    #[test]
    fn empty_env_is_treated_as_unset() {
        let _guard = env_lock();
        clear_env();
        let cfg = config_with_catalog("http://config-uri/catalog", "config-wh");
        std::env::set_var("RLEAN_DATA_CATALOG", "   ");
        let resolved = resolve(&cfg).unwrap();
        clear_env();
        assert_eq!(resolved.uri, "http://config-uri/catalog");
    }

    #[test]
    fn namespace_env_and_config_resolve() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_namespace: Some("lean_dev".to_string()),
            ..config_with_catalog("http://c/catalog", "wh")
        };
        assert_eq!(resolve(&cfg).unwrap().namespace, "lean_dev");
        std::env::set_var("RLEAN_DATA_NAMESPACE", "scratch");
        let ns = resolve(&cfg).unwrap().namespace;
        clear_env();
        assert_eq!(ns, "scratch");
    }

    #[test]
    fn data_refresh_secs_defaults_when_unset() {
        let _guard = env_lock();
        clear_env();
        let cfg = config_with_catalog("http://c/catalog", "wh");
        assert_eq!(
            resolve(&cfg).unwrap().data_refresh_secs,
            rlean_storage::DEFAULT_DATA_REFRESH_SECS
        );
    }

    #[test]
    fn missing_data_s3_endpoint_errors() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_catalog: Some("http://c/catalog".to_string()),
            data_warehouse: Some("wh".to_string()),
            ..GlobalConfig::default()
        };
        let error = resolve(&cfg).unwrap_err().to_string();
        assert!(error.contains("data_s3_endpoint"));
    }

    #[test]
    fn missing_data_s3_region_errors() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_s3_region: None,
            ..config_with_catalog("http://c/catalog", "wh")
        };
        let error = resolve(&cfg).unwrap_err().to_string();
        assert!(error.contains("data_s3_region"));
    }

    #[test]
    fn data_s3_config_from_config_is_required_and_complete() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_s3_endpoint: Some("http://127.0.0.1:8333".to_string()),
            data_s3_region: Some("us-east-1".to_string()),
            data_s3_access_key_id: Some("VG_KEY".to_string()),
            data_s3_secret_access_key: Some("VG_SECRET".to_string()),
            ..config_with_catalog("http://c/catalog", "wh")
        };
        let data_s3 = resolve(&cfg).unwrap().data_s3;
        assert_eq!(data_s3.endpoint, "http://127.0.0.1:8333");
        assert_eq!(data_s3.region, "us-east-1");
        assert_eq!(data_s3.access_key_id, "VG_KEY");
        assert_eq!(data_s3.secret_access_key, "VG_SECRET");
    }

    #[test]
    fn missing_data_s3_credentials_error() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_s3_endpoint: Some("http://127.0.0.1:8333".to_string()),
            data_s3_region: Some("us-east-1".to_string()),
            data_s3_access_key_id: None,
            data_s3_secret_access_key: None,
            ..config_with_catalog("http://c/catalog", "wh")
        };
        let error = resolve(&cfg).unwrap_err().to_string();
        assert!(error.contains("data_s3_access_key_id"));
    }

    #[test]
    fn data_s3_env_overrides_config() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_s3_endpoint: Some("http://config-endpoint:8333".to_string()),
            data_s3_region: Some("config-region".to_string()),
            data_s3_access_key_id: Some("CFG_KEY".to_string()),
            data_s3_secret_access_key: Some("CFG_SECRET".to_string()),
            ..config_with_catalog("http://c/catalog", "wh")
        };
        std::env::set_var("RLEAN_DATA_S3_ENDPOINT", "http://env-endpoint:8333");
        std::env::set_var("RLEAN_DATA_S3_REGION", "env-region");
        std::env::set_var("RLEAN_DATA_S3_ACCESS_KEY_ID", "ENV_KEY");
        let data_s3 = resolve(&cfg).unwrap().data_s3;
        clear_env();
        // Env wins per key; the unset secret falls back to config.
        assert_eq!(data_s3.endpoint, "http://env-endpoint:8333");
        assert_eq!(data_s3.region, "env-region");
        assert_eq!(data_s3.access_key_id, "ENV_KEY");
        assert_eq!(data_s3.secret_access_key, "CFG_SECRET");
    }

    #[test]
    fn data_refresh_secs_env_overrides_config() {
        let _guard = env_lock();
        clear_env();
        let cfg = GlobalConfig {
            data_refresh_secs: Some(15),
            ..config_with_catalog("http://c/catalog", "wh")
        };
        // Config value applies when the env var is unset.
        assert_eq!(resolve(&cfg).unwrap().data_refresh_secs, 15);
        // Env var wins, including 0 (recheck every read).
        std::env::set_var("RLEAN_DATA_REFRESH_SECS", "0");
        let secs = resolve(&cfg).unwrap().data_refresh_secs;
        clear_env();
        assert_eq!(secs, 0);
    }
}
