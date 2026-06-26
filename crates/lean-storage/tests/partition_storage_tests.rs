use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::{Market, NanosecondTimestamp, Resolution, Symbol, TickType, TimeSpan};
use lean_data::{TradeBar, TradeBarData};
use lean_storage::{ParquetReader, ParquetWriter, PathResolver, QueryParams, WriterConfig};
use rust_decimal_macros::dec;
use std::collections::HashMap;
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

#[test]
fn query_params_default_preserves_time_ordering_default() {
    assert!(QueryParams::default().order_by_time);
}

#[tokio::test]
async fn grouped_multi_partition_trade_reader_returns_chronological_rows() {
    let tmp = TempDir::new().unwrap();
    let resolver = PathResolver::new(tmp.path());
    let writer = ParquetWriter::new(WriterConfig::default());
    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let qqq = Symbol::create_equity("QQQ", &market);
    let day1 = date(2022, 5, 3);
    let day2 = date(2022, 5, 4);
    let day3 = date(2022, 5, 5);

    for day in [day1, day2, day3] {
        let path = resolver.market_data_partition(&spy, Resolution::Daily, TickType::Trade, day);
        writer
            .write_trade_bars(
                &[
                    TradeBar::new(
                        spy.clone(),
                        date_time(day, 16, 0, 0),
                        TimeSpan::ONE_DAY,
                        TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
                    ),
                    TradeBar::new(
                        qqq.clone(),
                        date_time(day, 16, 0, 0),
                        TimeSpan::ONE_DAY,
                        TradeBarData::new(dec!(200), dec!(200), dec!(200), dec!(200), dec!(1000)),
                    ),
                ],
                &path,
            )
            .unwrap();
    }

    let paths = [day3, day1, day2]
        .into_iter()
        .map(|day| resolver.market_data_partition(&spy, Resolution::Daily, TickType::Trade, day))
        .collect::<Vec<_>>();
    let mut params = QueryParams::new()
        .with_time_range(date_time(day1, 0, 0, 0), date_time(day3, 23, 59, 59))
        .with_symbols(vec![spy.id.sid, qqq.id.sid]);
    params.order_by_time = false;

    let grouped = ParquetReader::new()
        .read_trade_bar_partitions_grouped_async(
            &paths,
            &HashMap::from([(spy.id.sid, spy.clone()), (qqq.id.sid, qqq.clone())]),
            &params,
        )
        .await
        .unwrap();

    let spy_dates = grouped[&spy.id.sid]
        .iter()
        .map(|bar| bar.time.date_utc())
        .collect::<Vec<_>>();
    let qqq_dates = grouped[&qqq.id.sid]
        .iter()
        .map(|bar| bar.time.date_utc())
        .collect::<Vec<_>>();

    assert_eq!(spy_dates, vec![day1, day2, day3]);
    assert_eq!(qqq_dates, vec![day1, day2, day3]);
}
