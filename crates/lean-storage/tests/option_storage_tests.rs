/// Integration tests for option data storage in lean-storage.
///
/// These tests mirror the spirit of LEAN's C# LeanData path-generation
/// unit tests (found in Lean/Tests/Common/Data/LeanDataTests.cs) translated
/// to Rust, plus round-trip Parquet write/read tests.
use chrono::NaiveDate;
use lean_core::{Resolution, TickType};
use lean_storage::{
    schema::{OptionEodBar, OptionUniverseRow},
    ParquetReader, ParquetWriter, PathResolver, WriterConfig,
};
use rust_decimal_macros::dec;
use std::path::PathBuf;
use tempfile::TempDir;

// ─── helpers ────────────────────────────────────────────────────────────────

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

/// Build a minimal OptionEodBar for testing.
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

// ─── Path generation tests ───────────────────────────────────────────────────

/// Date-partitioned daily option path — one file per date for all underlyings:
///   option/usa/daily/trade/date=2021-04-30/data.parquet
#[test]
fn test_option_daily_trade_partition_path() {
    let pr = PathResolver::new("/data");
    let path = pr.option_partition(Resolution::Daily, TickType::Trade, date(2021, 4, 30));

    assert_eq!(
        path,
        PathBuf::from("/data/option/usa/daily/trade/date=2021-04-30/data.parquet"),
        "daily option trade partition path mismatch"
    );
}

/// Minute resolution option path:
///   option/usa/minute/trade/date=2021-04-30/data.parquet
#[test]
fn test_option_minute_trade_partition_path() {
    let pr = PathResolver::new("/data");
    let path = pr.option_partition(Resolution::Minute, TickType::Trade, date(2021, 4, 30));

    assert_eq!(
        path,
        PathBuf::from("/data/option/usa/minute/trade/date=2021-04-30/data.parquet"),
        "minute option trade partition path mismatch"
    );
}

/// Universe path:
///   option/usa/daily/universe/date=2021-01-01/data.parquet
#[test]
fn test_option_universe_partition_path() {
    let pr = PathResolver::new("/data");
    let path = pr.option_universe_partition(date(2021, 1, 1));

    assert_eq!(
        path,
        PathBuf::from("/data/option/usa/daily/universe/date=2021-01-01/data.parquet"),
        "option universe partition path mismatch"
    );
}

// ─── Parquet round-trip tests ─────────────────────────────────────────────────

/// Write OptionEodBar rows to a Parquet file and read them back; verify
/// that all fields survive the round trip (prices, dates, string columns).
#[test]
fn test_option_eod_bar_round_trip() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("option_eod_roundtrip.parquet");

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

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_option_eod_bars(&bars, &path).unwrap();

    assert!(path.exists(), "parquet file should have been created");

    let reader = ParquetReader::new();
    let roundtrip = reader.read_option_eod_bars(&[path]).unwrap();

    assert_eq!(roundtrip.len(), bars.len(), "row count should match");

    let put = roundtrip.iter().find(|b| b.right == "P").unwrap();
    assert_eq!(put.symbol_value, "SPY210430P00480000");
    assert_eq!(put.underlying, "SPY");
    assert_eq!(put.expiration, expiry);
    assert_eq!(put.strike, dec!(480.00));
    assert_eq!(put.open, dec!(3.50));
    assert_eq!(put.high, dec!(4.25));
    assert_eq!(put.low, dec!(3.10));
    assert_eq!(put.close, dec!(3.80));
    assert_eq!(put.volume, 1500);
    assert_eq!(put.bid, dec!(3.75));
    assert_eq!(put.ask, dec!(3.85));
    assert_eq!(put.bid_size, 10);
    assert_eq!(put.ask_size, 15);

    let call = roundtrip.iter().find(|b| b.right == "C").unwrap();
    assert_eq!(call.volume, 250);
    assert_eq!(call.close, dec!(1.50));
}

/// Write OptionUniverseRow rows to Parquet and read them back.
#[test]
fn test_option_universe_round_trip() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("option_universe_roundtrip.parquet");

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

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_option_universe(&rows, &path).unwrap();

    assert!(path.exists(), "parquet file should have been created");

    let reader = ParquetReader::new();
    let roundtrip = reader.read_option_universe(&[path]).unwrap();

    assert_eq!(roundtrip.len(), 2);
    let put = roundtrip.iter().find(|r| r.right == "P").unwrap();
    assert_eq!(put.symbol_value, "SPY210416P00400000");
    assert_eq!(put.underlying, "SPY");
    assert_eq!(put.date, date(2021, 1, 1));
    assert_eq!(put.expiration, expiry);
    assert_eq!(put.strike, dec!(480.00)); // from sample_universe_row

    let call = roundtrip.iter().find(|r| r.right == "C").unwrap();
    assert_eq!(call.strike, dec!(400.00));
}

/// Writing an empty slice should be a no-op (no file created).
#[test]
fn test_write_empty_option_eod_bars_noop() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("empty.parquet");
    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_option_eod_bars(&[], &path).unwrap();
    assert!(!path.exists(), "no file should be created for empty input");
}
