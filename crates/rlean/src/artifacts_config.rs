//! Resolve the artifact store mode + S3 settings from CLI flags, env vars, and
//! the `~/.rlean/config` store.
//!
//! Precedence (highest first):
//! 1. CLI flags (`--artifact-store`, `--artifact-s3`)
//! 2. Env vars (`RLEAN_ARTIFACT_STORE`, `RLEAN_ARTIFACT_S3`)
//! 3. `~/.rlean/config` (`artifact_store`, `artifact_s3`)
//!
//! Credentials come from the config store: the artifact-specific keys
//! (`artifact_s3_endpoint` / `_region` / `_access_key` / `_secret_key`) take
//! precedence, falling back to the shared `s3_*` keys so a single set of
//! credentials can serve both the data store and the artifact relay.
//!
//! Default is local-only. Nothing changes for users who do not opt in.

use anyhow::{bail, Result};
use rlean_engine::{ArtifactStoreMode, S3Settings};

use crate::config::GlobalConfig;

/// Resolved artifact configuration for a run.
pub(crate) struct ArtifactConfig {
    pub mode: ArtifactStoreMode,
    pub s3: Option<S3Settings>,
}

/// Resolve the mode string, applying CLI > env > config precedence.
fn resolve_mode(cli_mode: Option<&str>, config: &GlobalConfig) -> Result<ArtifactStoreMode> {
    let raw = cli_mode
        .map(str::to_string)
        .or_else(|| std::env::var("RLEAN_ARTIFACT_STORE").ok())
        .or_else(|| config.artifact_store.clone());
    match raw {
        None => Ok(ArtifactStoreMode::Local),
        Some(value) => ArtifactStoreMode::parse(&value).ok_or_else(|| {
            anyhow::anyhow!("invalid artifact store '{value}', expected local|s3|mirror")
        }),
    }
}

/// Resolve the `s3://bucket/prefix` destination string (CLI > env > config).
fn resolve_dest(cli_dest: Option<&str>, config: &GlobalConfig) -> Option<String> {
    cli_dest
        .map(str::to_string)
        .or_else(|| std::env::var("RLEAN_ARTIFACT_S3").ok())
        .or_else(|| config.artifact_s3.clone())
}

/// Parse `s3://bucket/prefix` into `(bucket, prefix)`. The prefix may be empty.
fn parse_s3_uri(uri: &str) -> Result<(String, String)> {
    let rest = uri.strip_prefix("s3://").ok_or_else(|| {
        anyhow::anyhow!("artifact S3 destination must start with s3://, got '{uri}'")
    })?;
    let rest = rest.trim_end_matches('/');
    let (bucket, prefix) = match rest.split_once('/') {
        Some((bucket, prefix)) => (bucket, prefix),
        None => (rest, ""),
    };
    if bucket.is_empty() {
        bail!("artifact S3 destination '{uri}' has an empty bucket");
    }
    Ok((bucket.to_string(), prefix.to_string()))
}

/// Resolve the full artifact configuration for a run.
pub(crate) fn resolve(
    cli_mode: Option<&str>,
    cli_s3: Option<&str>,
    config: &GlobalConfig,
) -> Result<ArtifactConfig> {
    let mode = resolve_mode(cli_mode, config)?;
    if mode == ArtifactStoreMode::Local {
        return Ok(ArtifactConfig { mode, s3: None });
    }

    let dest = resolve_dest(cli_s3, config).ok_or_else(|| {
        anyhow::anyhow!(
            "artifact store '{}' requires an S3 destination; set --artifact-s3 s3://bucket/prefix, \
             RLEAN_ARTIFACT_S3, or `rlean config set artifact_s3 s3://bucket/prefix`",
            match mode {
                ArtifactStoreMode::S3 => "s3",
                ArtifactStoreMode::Mirror => "mirror",
                ArtifactStoreMode::Local => unreachable!(),
            }
        )
    })?;
    let (bucket, prefix) = parse_s3_uri(&dest)?;

    let s3 = S3Settings {
        bucket,
        prefix,
        region: config
            .artifact_s3_region
            .clone()
            .or_else(|| config.s3_region.clone()),
        endpoint: config
            .artifact_s3_endpoint
            .clone()
            .or_else(|| config.s3_endpoint.clone()),
        access_key: config
            .artifact_s3_access_key
            .clone()
            .or_else(|| config.s3_access_key.clone()),
        secret_key: config
            .artifact_s3_secret_key
            .clone()
            .or_else(|| config.s3_secret_key.clone()),
    };

    Ok(ArtifactConfig { mode, s3: Some(s3) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> GlobalConfig {
        GlobalConfig::default()
    }

    #[test]
    fn default_is_local() {
        let cfg = base_config();
        let resolved = resolve(None, None, &cfg).unwrap();
        assert_eq!(resolved.mode, ArtifactStoreMode::Local);
        assert!(resolved.s3.is_none());
    }

    #[test]
    fn parse_s3_uri_bucket_and_prefix() {
        assert_eq!(
            parse_s3_uri("s3://my-bucket/runs/nested").unwrap(),
            ("my-bucket".to_string(), "runs/nested".to_string())
        );
        assert_eq!(
            parse_s3_uri("s3://my-bucket").unwrap(),
            ("my-bucket".to_string(), String::new())
        );
        assert!(parse_s3_uri("my-bucket/runs").is_err());
        assert!(parse_s3_uri("s3:///runs").is_err());
    }

    #[test]
    fn mirror_mode_requires_destination() {
        let mut cfg = base_config();
        cfg.artifact_store = Some("mirror".to_string());
        assert!(resolve(None, None, &cfg).is_err());
    }

    #[test]
    fn cli_beats_config_and_credentials_fall_back() {
        let mut cfg = base_config();
        cfg.artifact_store = Some("local".to_string());
        cfg.s3_access_key = Some("shared-key".to_string());
        cfg.s3_region = Some("us-east-1".to_string());
        // CLI overrides the config's local mode.
        let resolved = resolve(Some("mirror"), Some("s3://bucket/prefix"), &cfg).unwrap();
        assert_eq!(resolved.mode, ArtifactStoreMode::Mirror);
        let s3 = resolved.s3.unwrap();
        assert_eq!(s3.bucket, "bucket");
        assert_eq!(s3.prefix, "prefix");
        // Credentials fall back to the shared s3_* keys.
        assert_eq!(s3.access_key.as_deref(), Some("shared-key"));
        assert_eq!(s3.region.as_deref(), Some("us-east-1"));
    }

    #[test]
    fn artifact_specific_credentials_win() {
        let mut cfg = base_config();
        cfg.s3_access_key = Some("shared".to_string());
        cfg.artifact_s3_access_key = Some("artifact".to_string());
        let resolved = resolve(Some("s3"), Some("s3://b/p"), &cfg).unwrap();
        assert_eq!(resolved.s3.unwrap().access_key.as_deref(), Some("artifact"));
    }

    #[test]
    fn invalid_mode_errors() {
        let cfg = base_config();
        assert!(resolve(Some("nope"), None, &cfg).is_err());
    }
}
