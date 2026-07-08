use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::{Market, NanosecondTimestamp, Resolution, Symbol, TickType, TimeSpan};
use lean_data::{TradeBar, TradeBarData};
use lean_storage::{
    iceberg_store::{MARKET_QUOTE_BARS, MARKET_TRADE_BARS},
    IcebergStore, MarketPartitionDayQuery, QueryParams,
};
use rust_decimal_macros::dec;
use std::collections::HashMap;
use tempfile::TempDir;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn date_time(date: NaiveDate, h: u32, m: u32, s: u32) -> NanosecondTimestamp {
    NanosecondTimestamp::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap()))
}

fn days_since_epoch(value: NaiveDate) -> i32 {
    value.signed_duration_since(date(1970, 1, 1)).num_days() as i32
}

#[test]
fn query_params_default_preserves_time_ordering_default() {
    assert!(QueryParams::default().order_by_time);
}

#[tokio::test]
async fn iceberg_trade_bar_scan_returns_chronological_rows_by_symbol() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let qqq = Symbol::create_equity("QQQ", &market);
    let day1 = date(2022, 5, 3);
    let day2 = date(2022, 5, 4);
    let day3 = date(2022, 5, 5);

    for day in [day1, day2, day3] {
        store
            .append_trade_bars(
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
                lean_core::SecurityType::Equity,
                market.as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
    }

    let mut params = QueryParams::new()
        .with_bar_range(
            date_time(day1, 0, 0, 0),
            date_time(day3 + chrono::Duration::days(1), 23, 59, 59),
        )
        .with_symbols(vec![spy.id.sid, qqq.id.sid]);
    params.order_by_time = false;

    let grouped = store
        .scan_trade_bar_partitions_grouped(
            &HashMap::from([(spy.id.sid, spy.clone()), (qqq.id.sid, qqq.clone())]),
            Resolution::Daily,
            TickType::Trade,
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

#[tokio::test]
async fn market_partition_days_tracks_appended_market_partitions() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let day1 = date(2022, 5, 3);
    let day2 = date(2022, 5, 4);
    let day3 = date(2022, 5, 5);

    store
        .append_trade_bars(
            &[
                TradeBar::new(
                    spy.clone(),
                    date_time(day1, 9, 31, 0),
                    TimeSpan::ONE_MINUTE,
                    TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
                ),
                TradeBar::new(
                    spy.clone(),
                    date_time(day2, 9, 31, 0),
                    TimeSpan::ONE_MINUTE,
                    TradeBarData::new(dec!(101), dec!(101), dec!(101), dec!(101), dec!(1000)),
                ),
            ],
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            TickType::Trade,
        )
        .await
        .unwrap();

    let first_days = store
        .market_partition_days(MarketPartitionDayQuery::new(
            MARKET_TRADE_BARS,
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            spy.id.sid,
            days_since_epoch(day1),
            days_since_epoch(day3),
        ))
        .await
        .unwrap();
    assert_eq!(
        first_days,
        [days_since_epoch(day1), days_since_epoch(day2)]
            .into_iter()
            .collect()
    );

    store
        .append_trade_bars(
            &[TradeBar::new(
                spy.clone(),
                date_time(day3, 9, 31, 0),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(102), dec!(102), dec!(102), dec!(102), dec!(1000)),
            )],
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            TickType::Trade,
        )
        .await
        .unwrap();

    let updated_days = store
        .market_partition_days(MarketPartitionDayQuery::new(
            MARKET_TRADE_BARS,
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            spy.id.sid,
            days_since_epoch(day1),
            days_since_epoch(day3),
        ))
        .await
        .unwrap();
    assert_eq!(
        updated_days,
        [
            days_since_epoch(day1),
            days_since_epoch(day2),
            days_since_epoch(day3),
        ]
        .into_iter()
        .collect()
    );

    let quote_days = store
        .market_partition_days(MarketPartitionDayQuery::new(
            MARKET_QUOTE_BARS,
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            spy.id.sid,
            days_since_epoch(day1),
            days_since_epoch(day3),
        ))
        .await
        .unwrap();
    assert!(quote_days.is_empty());
}

#[tokio::test]
async fn warmed_market_partition_index_tracks_appends() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let day1 = date(2022, 5, 3);
    let day2 = date(2022, 5, 4);

    store
        .append_trade_bars(
            &[TradeBar::new(
                spy.clone(),
                date_time(day1, 9, 31, 0),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
            )],
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            TickType::Trade,
        )
        .await
        .unwrap();

    store
        .warm_market_partition_index(MARKET_TRADE_BARS)
        .await
        .unwrap();

    store
        .append_trade_bars(
            &[TradeBar::new(
                spy.clone(),
                date_time(day2, 9, 31, 0),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(101), dec!(101), dec!(101), dec!(101), dec!(1000)),
            )],
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            TickType::Trade,
        )
        .await
        .unwrap();

    let warmed_days = store
        .market_partition_days(MarketPartitionDayQuery::new(
            MARKET_TRADE_BARS,
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Minute,
            spy.id.sid,
            days_since_epoch(day1),
            days_since_epoch(day2),
        ))
        .await
        .unwrap();

    assert_eq!(
        warmed_days,
        [days_since_epoch(day1), days_since_epoch(day2)]
            .into_iter()
            .collect()
    );
}

#[tokio::test]
async fn daily_trade_bar_is_partitioned_by_end_time_day() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let start_day = date(2024, 1, 16);
    let end_day = date(2024, 1, 17);
    let bar = TradeBar::new(
        spy.clone(),
        date_time(start_day, 20, 0, 0),
        TimeSpan::ONE_DAY,
        TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
    );

    store
        .append_trade_bars(
            &[bar],
            lean_core::SecurityType::Equity,
            market.as_str(),
            Resolution::Daily,
            TickType::Trade,
        )
        .await
        .unwrap();

    let params = QueryParams::new()
        .with_day_range(date_time(end_day, 0, 0, 0), date_time(end_day, 23, 59, 59))
        .with_bar_range(date_time(end_day, 0, 0, 0), date_time(end_day, 23, 59, 59))
        .with_symbols(vec![spy.id.sid]);
    let grouped = store
        .scan_trade_bar_partitions_grouped(
            &HashMap::from([(spy.id.sid, spy.clone())]),
            Resolution::Daily,
            TickType::Trade,
            &params,
        )
        .await
        .unwrap();

    let rows = grouped.get(&spy.id.sid).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].time.date_utc(), start_day);
    assert_eq!(rows[0].end_time.date_utc(), end_day);
}

#[tokio::test]
async fn appending_same_trade_bar_twice_does_not_duplicate_key() {
    let tmp = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(tmp.path()).await.unwrap();
    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let day = date(2024, 1, 16);
    let storage_day = date(2024, 1, 17);
    let bar = TradeBar::new(
        spy.clone(),
        date_time(day, 16, 0, 0),
        TimeSpan::ONE_DAY,
        TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
    );

    for _ in 0..2 {
        store
            .append_trade_bars(
                std::slice::from_ref(&bar),
                lean_core::SecurityType::Equity,
                market.as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
    }

    let params = QueryParams::new()
        .with_day_range(
            date_time(storage_day, 0, 0, 0),
            date_time(storage_day, 23, 59, 59),
        )
        .with_bar_range(
            date_time(storage_day, 0, 0, 0),
            date_time(storage_day, 23, 59, 59),
        )
        .with_symbols(vec![spy.id.sid]);
    let grouped = store
        .scan_trade_bar_partitions_grouped(
            &HashMap::from([(spy.id.sid, spy.clone())]),
            Resolution::Daily,
            TickType::Trade,
            &params,
        )
        .await
        .unwrap();

    assert_eq!(grouped.get(&spy.id.sid).unwrap().len(), 1);
}
