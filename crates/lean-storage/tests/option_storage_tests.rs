use chrono::NaiveDate;
use lean_storage::schema::{OptionEodBar, OptionUniverseRow};
use lean_storage::IcebergStore;
use rust_decimal_macros::dec;
use tempfile::TempDir;

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
async fn option_eod_bars_round_trip_through_iceberg() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
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
async fn option_universe_round_trip_through_iceberg() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
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
async fn option_tables_filter_by_underlying() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
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
