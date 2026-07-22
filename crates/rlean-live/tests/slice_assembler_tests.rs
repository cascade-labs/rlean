use rlean_core::{DateTime, Market, Resolution, Symbol, TimeSpan};
use rlean_data::{LiveDataItem, Slice, SubscriptionDataConfig};
use rlean_data_tables::{TradeBar, TradeBarData};
use rlean_live::LiveSliceAssembler;
use rust_decimal_macros::dec;

fn symbol(ticker: &str) -> Symbol {
    Symbol::create_crypto(ticker, &Market::binance())
}

fn bar(ticker: &str, time: DateTime, close: i64) -> TradeBar {
    let _config = SubscriptionDataConfig::new_crypto(symbol(ticker), Resolution::Minute);
    TradeBar::new(
        symbol(ticker),
        time,
        TimeSpan::ONE_MINUTE,
        TradeBarData::new(
            dec!(100),
            dec!(110),
            dec!(90),
            rust_decimal::Decimal::from(close),
            dec!(1),
        ),
    )
}

fn bar_symbols(slice: &Slice) -> Vec<String> {
    let mut symbols: Vec<_> = slice
        .bars
        .values()
        .map(|bar| bar.symbol.value.to_string())
        .collect();
    symbols.sort();
    symbols
}

#[test]
fn same_frontier_items_are_emitted_in_one_slice() {
    let mut assembler = LiveSliceAssembler::new();

    assembler.enqueue(LiveDataItem::TradeBar(bar("BTCUSDT", DateTime::EPOCH, 101)));
    assembler.enqueue(LiveDataItem::TradeBar(bar("ETHUSDT", DateTime::EPOCH, 202)));

    let next_time = DateTime::EPOCH + TimeSpan::ONE_MINUTE;
    assembler.enqueue(LiveDataItem::TradeBar(bar("SOLUSDT", next_time, 303)));
    let frontier = next_time + TimeSpan::ONE_MINUTE;
    let emitted = assembler.advance(frontier).unwrap();

    assert_eq!(emitted.time, frontier);
    assert_eq!(bar_symbols(&emitted), vec!["BTCUSDT", "ETHUSDT", "SOLUSDT"]);
}

#[test]
fn idle_flush_only_emits_completed_frontiers() {
    let mut assembler = LiveSliceAssembler::new();
    let frontier = DateTime::EPOCH + TimeSpan::from_hours(1);

    assembler.enqueue(LiveDataItem::TradeBar(bar(
        "BTCUSDT",
        frontier - TimeSpan::ONE_MINUTE,
        101,
    )));
    assert!(assembler
        .advance(frontier - TimeSpan::from_secs(1))
        .is_none());

    let ready = assembler.advance(frontier).unwrap();
    assert_eq!(ready.time, frontier);
    assert_eq!(bar_symbols(&ready), vec!["BTCUSDT"]);
}

#[test]
fn late_provider_data_is_delivered_at_the_monotonic_live_frontier() {
    let mut assembler = LiveSliceAssembler::new();
    let live_frontier = DateTime::EPOCH + TimeSpan::from_hours(2);

    assembler.enqueue(LiveDataItem::TradeBar(bar(
        "BTCUSDT",
        live_frontier - TimeSpan::ONE_MINUTE,
        101,
    )));
    let first = assembler.advance(live_frontier).unwrap();
    assert_eq!(first.time, live_frontier);

    let stale_source_time = live_frontier - TimeSpan::from_hours(1);
    assembler.enqueue(LiveDataItem::TradeBar(bar(
        "ETHUSDT",
        stale_source_time,
        202,
    )));
    let late = assembler
        .advance(live_frontier - TimeSpan::from_secs(1))
        .unwrap();

    assert_eq!(late.time, live_frontier);
    assert_eq!(bar_symbols(&late), vec!["ETHUSDT"]);
    assert_eq!(late.bars.values().next().unwrap().time, stale_source_time);
}

#[test]
fn future_provider_data_waits_for_the_live_frontier() {
    let mut assembler = LiveSliceAssembler::new();
    let live_frontier = DateTime::EPOCH + TimeSpan::from_hours(2);
    let future_time = live_frontier + TimeSpan::ONE_MINUTE;

    assembler.enqueue(LiveDataItem::TradeBar(bar("BTCUSDT", future_time, 101)));

    assert!(assembler.advance(live_frontier).is_none());
    let ready = assembler
        .advance(future_time + TimeSpan::ONE_MINUTE)
        .unwrap();
    assert_eq!(ready.time, future_time + TimeSpan::ONE_MINUTE);
    assert_eq!(bar_symbols(&ready), vec!["BTCUSDT"]);
}
