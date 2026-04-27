use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::{Market, NanosecondTimestamp, Resolution, Symbol, TickType, TimeSpan};
use lean_data::{TradeBar, TradeBarData};
use lean_storage::{ParquetReader, ParquetWriter, PathResolver, QueryParams, WriterConfig};
use rust_decimal_macros::dec;
use tempfile::TempDir;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn date_time(date: NaiveDate, h: u32, m: u32, s: u32) -> NanosecondTimestamp {
    NanosecondTimestamp::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap()))
}

fn bar(ticker: &str, date: NaiveDate, close: rust_decimal::Decimal) -> TradeBar {
    TradeBar::new(
        Symbol::create_equity(ticker, &Market::usa()),
        date_time(date, 9, 30, 0),
        TimeSpan::from_nanos(60_000_000_000),
        TradeBarData::new(close, close, close, close, dec!(1000)),
    )
}

#[test]
fn market_data_partition_path_is_date_partitioned() {
    let resolver = PathResolver::new("/data");
    let path = resolver.market_data_partition(
        &Symbol::create_equity("SPY", &Market::usa()),
        Resolution::Minute,
        TickType::Trade,
        date(2022, 5, 3),
    );

    assert_eq!(
        path,
        std::path::PathBuf::from("/data/equity/usa/minute/trade/date=2022-05-03/data.parquet")
    );
}

#[test]
fn merge_trade_partition_preserves_existing_symbols_and_replaces_symbol_rows() {
    let tmp = TempDir::new().unwrap();
    let resolver = PathResolver::new(tmp.path());
    let day = date(2022, 5, 3);
    let path = resolver.market_data_partition(
        &Symbol::create_equity("SPY", &Market::usa()),
        Resolution::Minute,
        TickType::Trade,
        day,
    );
    let writer = ParquetWriter::new(WriterConfig::default());

    writer
        .merge_trade_bar_partition(
            &[bar("SPY", day, dec!(100)), bar("QQQ", day, dec!(200))],
            &path,
        )
        .unwrap();
    writer
        .merge_trade_bar_partition(&[bar("SPY", day, dec!(101))], &path)
        .unwrap();

    let reader = ParquetReader::new();
    let rows = reader
        .read_trade_bar_partition(
            &path,
            &Symbol::create_equity("SPY", &Market::usa()),
            &QueryParams::new(),
        )
        .unwrap();

    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .find(|row| row.symbol.value == "SPY")
            .unwrap()
            .close,
        dec!(101)
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.symbol.value == "QQQ")
            .unwrap()
            .close,
        dec!(200)
    );
}
