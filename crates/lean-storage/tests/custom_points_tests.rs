use std::collections::HashMap;

use chrono::NaiveDate;
use lean_core::{DateTime, NanosecondTimestamp};
use lean_data::{CustomDataPoint, CustomDataQuery};
use lean_storage::iceberg_store::CUSTOM_POINTS;
use lean_storage::IcebergStore;
use lean_storage::{RestCatalogConfig, SigV4Config};
use rust_decimal::Decimal;

fn end_time(date: NaiveDate) -> DateTime {
    DateTime::from(NanosecondTimestamp(
        date.and_hms_opt(16, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap(),
    ))
}

fn point(date: NaiveDate, value: i64, symbol: Option<&str>) -> CustomDataPoint {
    let mut fields = HashMap::new();
    // Distinct id keeps same-timestamp points from being deduped on append.
    fields.insert(
        "id".to_string(),
        serde_json::Value::String(value.to_string()),
    );
    if let Some(symbol) = symbol {
        // Providers still surface the original column in fields.
        fields.insert(
            "usymbol".to_string(),
            serde_json::Value::String(symbol.to_string()),
        );
    }
    CustomDataPoint::new(date, Some(end_time(date)), Decimal::from(value), fields)
        .with_symbol(symbol.map(str::to_string))
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn custom_point_symbol_round_trips_through_iceberg() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(CUSTOM_POINTS).await.unwrap();
    let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();

    store
        .append_custom_points("fixture", "ALT", &[point(day, 10, Some("aapl"))])
        .await
        .unwrap();

    let scanned = store
        .scan_custom_points_range("fixture", "ALT", day, day)
        .await
        .unwrap();
    assert_eq!(scanned.len(), 1);
    // Stored uppercased and read back as the canonical symbol.
    assert_eq!(scanned[0].symbol.as_deref(), Some("AAPL"));
    // Original provider column stays in fields.
    assert_eq!(
        scanned[0].fields.get("usymbol").and_then(|v| v.as_str()),
        Some("aapl")
    );
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn custom_query_symbols_filter_matches_point_symbol() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(CUSTOM_POINTS).await.unwrap();
    let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();

    store
        .append_custom_points(
            "fixture",
            "ALT",
            &[point(day, 10, Some("AAPL")), point(day, 20, Some("MSFT"))],
        )
        .await
        .unwrap();

    let query = CustomDataQuery {
        symbols: Some(vec!["msft".to_string()]),
        ..Default::default()
    };
    let filtered = store
        .scan_custom_points_range_with_query("fixture", "ALT", day, day, Some(&query))
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].symbol.as_deref(), Some("MSFT"));
}

fn snapshot_row(date: NaiveDate, symbol: &str) -> CustomDataPoint {
    // EOD snapshot shape: no provider row id, every ticker's row shares the
    // same 16:00 stamp — only the symbol distinguishes rows.
    let mut fields = HashMap::new();
    fields.insert(
        "usymbol".to_string(),
        serde_json::Value::String(symbol.to_string()),
    );
    CustomDataPoint::new(date, Some(end_time(date)), Decimal::from(1), fields)
        .with_symbol(Some(symbol.to_string()))
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn same_timestamp_idless_rows_keep_all_symbols_and_stay_idempotent() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(CUSTOM_POINTS).await.unwrap();
    let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();

    let rows: Vec<CustomDataPoint> = ["AAPL", "MSFT", "OMI", "SPY"]
        .iter()
        .map(|s| snapshot_row(day, s))
        .collect();

    store
        .append_custom_points("tradealert", "snapshot", &rows)
        .await
        .unwrap();
    let scanned = store
        .scan_custom_points_range("tradealert", "snapshot", day, day)
        .await
        .unwrap();
    // Every ticker's row survives the append despite identical timestamps.
    assert_eq!(scanned.len(), 4);

    // Re-appending the same day is idempotent (cross-append dedupe).
    store
        .append_custom_points("tradealert", "snapshot", &rows)
        .await
        .unwrap();
    let rescanned = store
        .scan_custom_points_range("tradealert", "snapshot", day, day)
        .await
        .unwrap();
    assert_eq!(rescanned.len(), 4);
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn same_timestamp_no_id_points_dedupe_by_symbol_not_time() {
    // EOD snapshot shape: every ticker's row shares one 16:00 stamp and has no
    // provider id — only the canonical symbol distinguishes rows. A time-only
    // dedupe key collapses ~3,900 rows/day to one.
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(CUSTOM_POINTS).await.unwrap();
    let date = NaiveDate::from_ymd_opt(2021, 6, 1).unwrap();
    let points: Vec<CustomDataPoint> = ["OMI", "GDDY", "ASR"]
        .iter()
        .map(|sym| {
            let mut fields = HashMap::new();
            fields.insert(
                "usymbol".to_string(),
                serde_json::Value::String(sym.to_string()),
            );
            CustomDataPoint::new(date, Some(end_time(date)), Decimal::from(1), fields)
                .with_symbol(Some(sym.to_string()))
        })
        .collect();
    store
        .append_custom_points("tradealert", "snapshot", &points)
        .await
        .unwrap();
    let rows = store
        .scan_custom_points_range("tradealert", "snapshot", date, date)
        .await
        .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "same-timestamp no-id points must not collapse: got {:?}",
        rows.iter().map(|p| p.symbol.clone()).collect::<Vec<_>>()
    );
}

/// Connect to a REST Iceberg catalog for integration testing.
///
/// These tests need a live REST catalog (e.g. AWS S3 Tables): they are marked
/// `#[ignore]` and only run when opted in. Set `RLEAN_TEST_CATALOG` to the
/// catalog base URI to enable them; `RLEAN_TEST_WAREHOUSE` supplies the
/// warehouse identifier, and `RLEAN_TEST_SIGV4_REGION` (+ optional
/// `RLEAN_TEST_SIGV4_NAME`, default `s3tables`) turns on SigV4 signing.
/// `RLEAN_TEST_NAMESPACE` selects the Iceberg namespace (default `lean_dev`, an
/// isolated scratch namespace that never touches the production `lean` tables).
/// When `RLEAN_TEST_CATALOG` is unset the helper returns `None` and the test skips.
///
/// The scratch namespace persists across runs, so each gated test that asserts
/// on a table's full contents resets that table (drop + recreate) after
/// connecting to start from a known-empty state.
async fn connect_test_store() -> Option<IcebergStore> {
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
    // Default to an isolated scratch namespace so gated tests never write into
    // the production `lean` tables; override with RLEAN_TEST_NAMESPACE.
    let namespace = std::env::var("RLEAN_TEST_NAMESPACE")
        .ok()
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| "lean_dev".to_string());
    Some(
        IcebergStore::connect(RestCatalogConfig {
            uri,
            warehouse,
            sigv4,
            namespace,
        })
        .await
        .expect("failed to connect to the test REST catalog"),
    )
}
