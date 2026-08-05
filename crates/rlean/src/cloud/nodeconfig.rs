//! Node config generation for `rlean cloud install`.
//!
//! Produces the `~/.rlean/config` written on a remote node from the operator's
//! local `GlobalConfig`. The local config is READ ONLY — never modified. The
//! node runs with `artifact_store = "mirror"` and a node-local workspace path
//! so live runs mirror artifacts to the same S3 the operator uses while
//! keeping a working local copy on the node.

use crate::config::GlobalConfig;

/// Build the node `GlobalConfig` from the operator's local config plus the
/// node's absolute `$HOME`.
///
/// - `workspace`     → `<node_home>/rlean-cloud/workspace`
/// - `data_sidecar*` → copied verbatim
/// - `artifact_store`→ `"mirror"` (overrides the local, typically `"local"`)
/// - `artifact_s3*`  → copied verbatim from the local config (operator creds),
///   except the endpoint, which `artifact_endpoint` overrides when given — a
///   node often needs a different route to the same object store than the
///   control machine (e.g. an OCI bucket's `.private.` endpoint is reachable
///   from the operator's VPN but not from the node's subnet, while the public
///   form of the same endpoint is).
/// - `default_language` → copied from local (already defaults to `"python"`)
///
pub(crate) fn node_config_from_local(
    local: &GlobalConfig,
    node_home: &str,
    artifact_endpoint: Option<&str>,
) -> GlobalConfig {
    let node_home = node_home.trim_end_matches('/');
    GlobalConfig {
        default_language: local.default_language.clone(),
        data_sidecar: local.data_sidecar.clone(),
        data_sidecar_token: local.data_sidecar_token.clone(),
        verglas_endpoint: local.verglas_endpoint.clone(),
        verglas_token: local.verglas_token.clone(),
        artifact_store: Some("mirror".to_string()),
        artifact_s3: local.artifact_s3.clone(),
        artifact_s3_endpoint: artifact_endpoint
            .map(str::to_string)
            .or_else(|| local.artifact_s3_endpoint.clone()),
        artifact_s3_region: local.artifact_s3_region.clone(),
        artifact_s3_access_key: local.artifact_s3_access_key.clone(),
        artifact_s3_secret_key: local.artifact_s3_secret_key.clone(),
        workspace: Some(format!("{node_home}/rlean-cloud/workspace")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_with_artifacts() -> GlobalConfig {
        GlobalConfig {
            default_language: "python".to_string(),
            data_sidecar: None,
            data_sidecar_token: None,
            verglas_endpoint: Some("http://127.0.0.1:8334".to_string()),
            verglas_token: Some("vgk_test".to_string()),
            // Local operator runs artifact_store=local but supplies artifact creds.
            artifact_store: Some("local".to_string()),
            artifact_s3: Some("s3://runs-bucket/rlean".to_string()),
            artifact_s3_endpoint: Some("https://s3.example".to_string()),
            artifact_s3_region: Some("us-east-1".to_string()),
            artifact_s3_access_key: Some("AKIA_ART".to_string()),
            artifact_s3_secret_key: Some("SECRET_ART".to_string()),
            workspace: Some("/Users/op/strategies".to_string()),
        }
    }

    #[test]
    fn node_config_overrides_artifact_store_and_workspace() {
        let node = node_config_from_local(&local_with_artifacts(), "/home/opc", None);

        assert_eq!(node.artifact_store.as_deref(), Some("mirror"));
        assert_eq!(
            node.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
    }

    #[test]
    fn node_config_copies_explicit_artifact_credentials() {
        let node = node_config_from_local(&local_with_artifacts(), "/home/opc", None);

        assert_eq!(node.artifact_s3.as_deref(), Some("s3://runs-bucket/rlean"));
        assert_eq!(
            node.artifact_s3_endpoint.as_deref(),
            Some("https://s3.example")
        );
        assert_eq!(node.artifact_s3_region.as_deref(), Some("us-east-1"));
        assert_eq!(node.artifact_s3_access_key.as_deref(), Some("AKIA_ART"));
        assert_eq!(
            node.verglas_endpoint.as_deref(),
            Some("http://127.0.0.1:8334")
        );
        assert_eq!(node.verglas_token.as_deref(), Some("vgk_test"));
        assert_eq!(node.artifact_s3_secret_key.as_deref(), Some("SECRET_ART"));
    }

    #[test]
    fn node_config_copies_language_and_handles_missing_artifacts() {
        let mut local = GlobalConfig {
            default_language: "csharp".to_string(),
            ..GlobalConfig::default()
        };
        local.artifact_s3 = None;
        let node = node_config_from_local(&local, "/home/opc/", None);

        assert_eq!(node.default_language, "csharp");
        // Missing artifact creds stay missing — we never invent values.
        assert!(node.artifact_s3.is_none());
        assert_eq!(
            node.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
    }

    #[test]
    fn node_config_serializes_with_expected_keys() {
        let node = node_config_from_local(&local_with_artifacts(), "/home/opc", None);
        let json = serde_json::to_string_pretty(&node).unwrap();
        // The struct uses kebab-case for un-renamed fields but keeps an explicit
        // `artifact_store` rename; the node must parse this exact shape back.
        assert!(json.contains("\"artifact_store\": \"mirror\""));
        assert!(!json.contains("data_catalog"));
        assert!(!json.contains("data-folder"));
        assert!(json.contains("\"default-language\": \"python\""));
        // No legacy generic S3 keys exist in the client config.
        assert!(!json.contains("\"s3_access_key\""));
        assert!(!json.contains("\"s3_bucket\""));
        assert!(!json.contains("\"s3_endpoint\""));
        // The artifact credentials DO carry over (proving it's not just absent).
        assert!(json.contains("\"artifact_s3_access_key\": \"AKIA_ART\""));
        assert!(json.contains("\"verglas_endpoint\": \"http://127.0.0.1:8334\""));
        assert!(json.contains("\"verglas_token\": \"vgk_test\""));
        // Round-trips back through GlobalConfig identically.
        let reparsed: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.artifact_store.as_deref(), Some("mirror"));
        assert_eq!(
            reparsed.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
    }

    #[test]
    fn node_config_endpoint_override_replaces_only_the_endpoint() {
        let node = node_config_from_local(
            &local_with_artifacts(),
            "/home/opc",
            Some("https://public.s3.example"),
        );
        assert_eq!(
            node.artifact_s3_endpoint.as_deref(),
            Some("https://public.s3.example")
        );
        // Everything else still comes from the local config.
        assert_eq!(node.artifact_s3.as_deref(), Some("s3://runs-bucket/rlean"));
        assert_eq!(node.artifact_s3_access_key.as_deref(), Some("AKIA_ART"));
    }
}
