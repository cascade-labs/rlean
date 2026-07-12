//! Node config generation for `rlean cloud install`.
//!
//! Produces the `~/.rlean/config` written on a remote node from the operator's
//! local `GlobalConfig`. The local config is READ ONLY — never modified. The
//! node runs with `artifact_store = "mirror"` and node-local data/workspace
//! paths so live runs mirror artifacts to the same S3 the operator uses while
//! keeping a working local copy on the node.

use crate::config::GlobalConfig;

/// Build the node `GlobalConfig` from the operator's local config plus the
/// node's absolute `$HOME`.
///
/// - `data_folder`   → `<node_home>/rlean-cloud/data`
/// - `workspace`     → `<node_home>/rlean-cloud/workspace`
/// - `datastore`     → `"file"`
/// - `artifact_store`→ `"mirror"` (overrides the local, typically `"local"`)
/// - `artifact_s3*`  → copied verbatim from the local config (operator creds)
/// - `default_language` → copied from local (already defaults to `"python"`)
///
/// The local `data_folder` (a control-machine path such as `/Volumes/...`) is
/// deliberately dropped; the local `s3_*` fields are left untouched on the
/// local config and are NOT copied into the node config (only the `artifact_s3*`
/// fields are, matching how the artifact relay reads its credentials).
pub(crate) fn node_config_from_local(local: &GlobalConfig, node_home: &str) -> GlobalConfig {
    let node_home = node_home.trim_end_matches('/');
    GlobalConfig {
        default_language: local.default_language.clone(),
        datastore: "file".to_string(),
        // Local `s3_*` connection settings are the control machine's; the node
        // relays via `artifact_s3*`, so leave the plain `s3_*` block empty.
        s3_access_key: None,
        s3_secret_key: None,
        s3_bucket: None,
        s3_endpoint: None,
        s3_region: None,
        artifact_store: Some("mirror".to_string()),
        artifact_s3: local.artifact_s3.clone(),
        artifact_s3_endpoint: local.artifact_s3_endpoint.clone(),
        artifact_s3_region: local.artifact_s3_region.clone(),
        artifact_s3_access_key: local.artifact_s3_access_key.clone(),
        artifact_s3_secret_key: local.artifact_s3_secret_key.clone(),
        data_folder: Some(format!("{node_home}/rlean-cloud/data")),
        workspace: Some(format!("{node_home}/rlean-cloud/workspace")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_with_artifacts() -> GlobalConfig {
        GlobalConfig {
            default_language: "python".to_string(),
            datastore: "file".to_string(),
            s3_access_key: Some("LOCALKEY".to_string()),
            s3_secret_key: Some("LOCALSECRET".to_string()),
            s3_bucket: Some("local-bucket".to_string()),
            s3_endpoint: Some("https://local.example".to_string()),
            s3_region: Some("us-west-2".to_string()),
            // Local operator runs artifact_store=local but supplies artifact creds.
            artifact_store: Some("local".to_string()),
            artifact_s3: Some("s3://runs-bucket/rlean".to_string()),
            artifact_s3_endpoint: Some("https://s3.example".to_string()),
            artifact_s3_region: Some("us-east-1".to_string()),
            artifact_s3_access_key: Some("AKIA_ART".to_string()),
            artifact_s3_secret_key: Some("SECRET_ART".to_string()),
            data_folder: Some("/Volumes/mac-data".to_string()),
            workspace: Some("/Users/op/strategies".to_string()),
        }
    }

    #[test]
    fn node_config_overrides_store_and_paths() {
        let node = node_config_from_local(&local_with_artifacts(), "/home/opc");

        assert_eq!(node.datastore, "file");
        assert_eq!(node.artifact_store.as_deref(), Some("mirror"));
        assert_eq!(
            node.data_folder.as_deref(),
            Some("/home/opc/rlean-cloud/data")
        );
        assert_eq!(
            node.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
        // The mac data folder is never leaked.
        assert_ne!(node.data_folder.as_deref(), Some("/Volumes/mac-data"));
    }

    #[test]
    fn node_config_copies_artifact_creds_only() {
        let node = node_config_from_local(&local_with_artifacts(), "/home/opc");

        assert_eq!(node.artifact_s3.as_deref(), Some("s3://runs-bucket/rlean"));
        assert_eq!(
            node.artifact_s3_endpoint.as_deref(),
            Some("https://s3.example")
        );
        assert_eq!(node.artifact_s3_region.as_deref(), Some("us-east-1"));
        assert_eq!(node.artifact_s3_access_key.as_deref(), Some("AKIA_ART"));
        assert_eq!(node.artifact_s3_secret_key.as_deref(), Some("SECRET_ART"));

        // The plain s3_* block is NOT carried over to the node config.
        assert!(node.s3_access_key.is_none());
        assert!(node.s3_secret_key.is_none());
        assert!(node.s3_bucket.is_none());
        assert!(node.s3_endpoint.is_none());
        assert!(node.s3_region.is_none());
    }

    #[test]
    fn node_config_copies_language_and_handles_missing_artifacts() {
        let mut local = GlobalConfig {
            default_language: "csharp".to_string(),
            ..GlobalConfig::default()
        };
        local.artifact_s3 = None;
        let node = node_config_from_local(&local, "/home/opc/");

        assert_eq!(node.default_language, "csharp");
        // Missing artifact creds stay missing — we never invent values.
        assert!(node.artifact_s3.is_none());
        // Trailing slash on node_home is normalized.
        assert_eq!(
            node.data_folder.as_deref(),
            Some("/home/opc/rlean-cloud/data")
        );
    }

    #[test]
    fn node_config_serializes_with_expected_keys() {
        let node = node_config_from_local(&local_with_artifacts(), "/home/opc");
        let json = serde_json::to_string_pretty(&node).unwrap();
        // The struct uses kebab-case for un-renamed fields but keeps an explicit
        // `artifact_store` rename; the node must parse this exact shape back.
        assert!(json.contains("\"artifact_store\": \"mirror\""));
        assert!(json.contains("\"datastore\": \"file\""));
        assert!(json.contains("\"data-folder\": \"/home/opc/rlean-cloud/data\""));
        assert!(json.contains("\"default-language\": \"python\""));
        // No local-only s3_* keys leak in. Match the JSON key form precisely so
        // the `artifact_s3_access_key` credential key isn't a false positive.
        assert!(!json.contains("\"s3_access_key\""));
        assert!(!json.contains("\"s3_bucket\""));
        assert!(!json.contains("\"s3_endpoint\""));
        // The artifact credentials DO carry over (proving it's not just absent).
        assert!(json.contains("\"artifact_s3_access_key\": \"AKIA_ART\""));
        // Round-trips back through GlobalConfig identically.
        let reparsed: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.artifact_store.as_deref(), Some("mirror"));
        assert_eq!(reparsed.datastore, "file");
        assert_eq!(
            reparsed.data_folder.as_deref(),
            Some("/home/opc/rlean-cloud/data")
        );
    }
}
