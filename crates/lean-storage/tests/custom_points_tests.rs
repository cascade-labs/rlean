use std::collections::HashMap;

use chrono::NaiveDate;
use lean_core::{DateTime, NanosecondTimestamp};
use lean_data::{CustomDataPoint, CustomDataQuery};
use lean_storage::IcebergStore;
use rust_decimal::Decimal;
use tempfile::TempDir;

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
async fn custom_point_symbol_round_trips_through_iceberg() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
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
async fn custom_query_symbols_filter_matches_point_symbol() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
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
