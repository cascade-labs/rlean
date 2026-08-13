//! Node config generation for `rlean cloud install`.
//!
//! Produces the `~/.rlean/config` written on a remote node from the operator's
//! local `GlobalConfig`. The local config is READ ONLY — never modified. The
//! node gets a node-local workspace path and copies Verglas / provider settings
//! from the operator.

use crate::config::GlobalConfig;

/// Build the node `GlobalConfig` from the operator's local config plus the
/// node's absolute `$HOME`.
///
/// - `workspace` → `<node_home>/rlean-cloud/workspace`
/// - `default_language` → copied from local (already defaults to `"python"`)
/// - `verglas_*` → copied from local
/// - `providers` → copied from local (native provider credentials)
pub(crate) fn node_config_from_local(local: &GlobalConfig, node_home: &str) -> GlobalConfig {
    let node_home = node_home.trim_end_matches('/');
    GlobalConfig {
        default_language: local.default_language.clone(),
        verglas_endpoint: local.verglas_endpoint.clone(),
        verglas_access_uri: local.verglas_access_uri.clone(),
        verglas_database: local.verglas_database.clone(),
        verglas_token: local.verglas_token.clone(),
        providers: local.providers.clone(),
        workspace: Some(format!("{node_home}/rlean-cloud/workspace")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_local() -> GlobalConfig {
        GlobalConfig {
            default_language: "python".to_string(),
            verglas_endpoint: Some("http://127.0.0.1:8334".to_string()),
            verglas_access_uri: Some("http://127.0.0.1:8345".to_string()),
            verglas_database: Some("rlean".to_string()),
            verglas_token: Some("vgk_test".to_string()),
            providers: Default::default(),
            workspace: Some("/Users/op/strategies".to_string()),
        }
    }

    #[test]
    fn node_config_overrides_workspace() {
        let node = node_config_from_local(&sample_local(), "/home/opc");
        assert_eq!(
            node.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
    }

    #[test]
    fn node_config_copies_verglas_and_providers() {
        let mut local = sample_local();
        local
            .set_provider_key("thetadata", "api_key", "td-key".into())
            .unwrap();
        let node = node_config_from_local(&local, "/home/opc");

        assert_eq!(
            node.verglas_endpoint.as_deref(),
            Some("http://127.0.0.1:8334")
        );
        assert_eq!(node.verglas_token.as_deref(), Some("vgk_test"));
        assert_eq!(node.verglas_database.as_deref(), Some("rlean"));
        assert_eq!(
            node.get_provider("thetadata")
                .get("api_key")
                .map(String::as_str),
            Some("td-key")
        );
    }

    #[test]
    fn node_config_copies_language_and_trims_home() {
        let local = GlobalConfig {
            default_language: "csharp".to_string(),
            ..GlobalConfig::default()
        };
        let node = node_config_from_local(&local, "/home/opc/");

        assert_eq!(node.default_language, "csharp");
        assert_eq!(
            node.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
    }

    #[test]
    fn node_config_serializes_with_expected_keys() {
        let node = node_config_from_local(&sample_local(), "/home/opc");
        let json = serde_json::to_string_pretty(&node).unwrap();
        assert!(!json.contains("data_catalog"));
        assert!(!json.contains("data-folder"));
        assert!(!json.contains("artifact_store"));
        assert!(json.contains("\"default-language\": \"python\""));
        assert!(json.contains("\"verglas_endpoint\": \"http://127.0.0.1:8334\""));
        assert!(json.contains("\"verglas_database\": \"rlean\""));
        assert!(json.contains("\"verglas_token\": \"vgk_test\""));
        let reparsed: GlobalConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reparsed.workspace.as_deref(),
            Some("/home/opc/rlean-cloud/workspace")
        );
    }
}
