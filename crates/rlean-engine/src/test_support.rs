//! Shared test-only helpers for connecting to a REST Iceberg catalog.
//!
//! There is no local/filesystem catalog anymore, so tests that exercise the
//! store need a live REST catalog (e.g. AWS S3 Tables). Such tests are marked
//! `#[ignore]` and skip themselves when the catalog is not configured, so
//! `cargo test` stays green in CI without AWS.

use std::sync::Arc;

use rlean_storage::{DataS3Config, IcebergStore, RestCatalogConfig, SigV4Config};

/// Connect to the test REST Iceberg catalog, or return `None` when it is not
/// configured.
///
/// Reads:
/// - `RLEAN_TEST_CATALOG`      REST catalog base URI (unset => `None`, test skips)
/// - `RLEAN_TEST_WAREHOUSE`    warehouse identifier / table-bucket ARN (required
///   when the catalog is set)
/// - `RLEAN_TEST_SIGV4_REGION` SigV4 signing region; when set, requests are
///   signed with SigV4
/// - `RLEAN_TEST_SIGV4_NAME`   SigV4 signing name (default `s3tables`)
/// - `RLEAN_TEST_NAMESPACE`    Iceberg namespace (default `lean_dev`, an isolated
///   scratch namespace so gated tests never touch the production `lean` tables)
/// - `RLEAN_TEST_S3_ENDPOINT`, `RLEAN_TEST_S3_REGION`,
///   `RLEAN_TEST_S3_ACCESS_KEY_ID`, `RLEAN_TEST_S3_SECRET_ACCESS_KEY`: the
///   required S3-compatible endpoint used for Iceberg data I/O
pub(crate) async fn connect_test_store() -> Option<Arc<IcebergStore>> {
    let uri = std::env::var("RLEAN_TEST_CATALOG")
        .ok()
        .filter(|v| !v.is_empty())?;
    let warehouse = std::env::var("RLEAN_TEST_WAREHOUSE")
        .expect("RLEAN_TEST_WAREHOUSE must be set when RLEAN_TEST_CATALOG is");
    let sigv4 = std::env::var("RLEAN_TEST_SIGV4_REGION")
        .ok()
        .filter(|region| !region.is_empty())
        .map(|region| SigV4Config {
            region,
            signing_name: std::env::var("RLEAN_TEST_SIGV4_NAME")
                .ok()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "s3tables".to_string()),
        });
    let namespace = std::env::var("RLEAN_TEST_NAMESPACE")
        .ok()
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| "lean_dev".to_string());
    let data_s3 = DataS3Config {
        endpoint: std::env::var("RLEAN_TEST_S3_ENDPOINT")
            .ok()
            .filter(|value| !value.is_empty())?,
        region: std::env::var("RLEAN_TEST_S3_REGION")
            .ok()
            .filter(|value| !value.is_empty())?,
        access_key_id: std::env::var("RLEAN_TEST_S3_ACCESS_KEY_ID")
            .ok()
            .filter(|value| !value.is_empty())?,
        secret_access_key: std::env::var("RLEAN_TEST_S3_SECRET_ACCESS_KEY")
            .ok()
            .filter(|value| !value.is_empty())?,
    };
    Some(Arc::new(
        IcebergStore::connect(RestCatalogConfig {
            uri,
            warehouse,
            sigv4,
            namespace,
            data_refresh_secs: 0,
            data_s3,
        })
        .await
        .expect("failed to connect to the test REST catalog"),
    ))
}
