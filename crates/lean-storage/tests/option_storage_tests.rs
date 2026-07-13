use chrono::NaiveDate;
use lean_storage::iceberg_store::{OPTION_EOD_BARS, OPTION_UNIVERSE};
use lean_storage::schema::{OptionEodBar, OptionUniverseRow};
use lean_storage::IcebergStore;
use lean_storage::{RestCatalogConfig, SigV4Config};
use rust_decimal_macros::dec;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn sample_eod_bar(underlying: &str, osi: &str, expiry: NaiveDate, right: &str) -> OptionEodBar {
    OptionEodBar {
        date: date(2021, 4, 30),
        symbol_value: osi.to_string(),
        underlying: underlying.to_string(),
        expiration: expiry,
        strike: dec!(480.00),
        right: right.to_string(),
        open: dec!(3.50),
        high: dec!(4.25),
        low: dec!(3.10),
        close: dec!(3.80),
        volume: 1500,
        bid: dec!(3.75),
        ask: dec!(3.85),
        bid_size: 10,
        ask_size: 15,
    }
}

fn sample_universe_row(underlying: &str, osi: &str, expiry: NaiveDate) -> OptionUniverseRow {
    OptionUniverseRow {
        date: date(2021, 1, 1),
        symbol_value: osi.to_string(),
        underlying: underlying.to_string(),
        expiration: expiry,
        strike: dec!(480.00),
        right: "P".to_string(),
    }
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn option_eod_bars_round_trip_through_iceberg() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(OPTION_EOD_BARS).await.unwrap();
    let expiry = date(2021, 4, 30);
    let bars = vec![
        sample_eod_bar("SPY", "SPY210430P00480000", expiry, "P"),
        OptionEodBar {
            date: date(2021, 4, 30),
            symbol_value: "SPY210430C00480000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(480.00),
            right: "C".to_string(),
            open: dec!(1.20),
            high: dec!(2.00),
            low: dec!(1.10),
            close: dec!(1.50),
            volume: 250,
            bid: dec!(1.45),
            ask: dec!(1.55),
            bid_size: 5,
            ask_size: 8,
        },
    ];

    store.append_option_eod_bars(&bars).await.unwrap();
    let roundtrip = store
        .scan_option_eod_bars(&["SPY".to_string()], date(2021, 4, 30))
        .await
        .unwrap();

    assert_eq!(roundtrip.len(), bars.len());
    let put = roundtrip.iter().find(|bar| bar.right == "P").unwrap();
    assert_eq!(put.symbol_value, "SPY210430P00480000");
    assert_eq!(put.underlying, "SPY");
    assert_eq!(put.expiration, expiry);
    assert_eq!(put.strike, dec!(480.00));
    assert_eq!(put.close, dec!(3.80));
    assert_eq!(put.volume, 1500);

    let call = roundtrip.iter().find(|bar| bar.right == "C").unwrap();
    assert_eq!(call.volume, 250);
    assert_eq!(call.close, dec!(1.50));
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn option_universe_round_trip_through_iceberg() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(OPTION_UNIVERSE).await.unwrap();
    let expiry = date(2021, 4, 16);
    let rows = vec![
        sample_universe_row("SPY", "SPY210416P00400000", expiry),
        OptionUniverseRow {
            date: date(2021, 1, 1),
            symbol_value: "SPY210416C00400000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(400.00),
            right: "C".to_string(),
        },
    ];

    store.append_option_universe(&rows).await.unwrap();
    let roundtrip = store
        .scan_option_universe(&["SPY".to_string()], date(2021, 1, 1))
        .await
        .unwrap();

    assert_eq!(roundtrip.len(), 2);
    assert!(roundtrip
        .iter()
        .any(|row| row.symbol_value == "SPY210416P00400000"));
    assert!(roundtrip
        .iter()
        .any(|row| row.symbol_value == "SPY210416C00400000"));
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn option_tables_filter_by_underlying() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(OPTION_UNIVERSE).await.unwrap();
    let expiry = date(2021, 4, 16);
    let rows = vec![
        sample_universe_row("SPY", "SPY210416P00480000", expiry),
        sample_universe_row("QQQ", "QQQ210416P00350000", expiry),
        sample_universe_row("AAPL", "AAPL210416P00150000", expiry),
    ];

    store.append_option_universe(&rows).await.unwrap();
    let filtered = store
        .scan_option_universe(&["SPY".to_string(), "AAPL".to_string()], date(2021, 1, 1))
        .await
        .unwrap();

    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().any(|row| row.underlying == "SPY"));
    assert!(filtered.iter().any(|row| row.underlying == "AAPL"));
    assert!(!filtered.iter().any(|row| row.underlying == "QQQ"));
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
            data_refresh_secs: 0,
            data_endpoint: None,
        })
        .await
        .expect("failed to connect to the test REST catalog"),
    )
}
