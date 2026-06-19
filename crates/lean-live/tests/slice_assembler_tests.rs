use lean_core::{DateTime, Market, Resolution, Symbol, TimeSpan};
use lean_data::{LiveDataItem, Slice, SubscriptionDataConfig, TradeBar, TradeBarData};
use lean_live::LiveSliceAssembler;
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
        .map(|bar| bar.symbol.value.clone())
        .collect();
    symbols.sort();
    symbols
}

#[test]
fn same_frontier_items_are_emitted_in_one_slice() {
    let mut assembler = LiveSliceAssembler::new();

    assert!(assembler
        .push(LiveDataItem::TradeBar(bar("BTCUSDT", DateTime::EPOCH, 101)))
        .is_empty());
    assert!(assembler
        .push(LiveDataItem::TradeBar(bar("ETHUSDT", DateTime::EPOCH, 202)))
        .is_empty());

    let next_time = DateTime::EPOCH + TimeSpan::ONE_MINUTE;
    let emitted = assembler.push(LiveDataItem::TradeBar(bar("SOLUSDT", next_time, 303)));

    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].time, DateTime::EPOCH + TimeSpan::ONE_MINUTE);
    assert_eq!(bar_symbols(&emitted[0]), vec!["BTCUSDT", "ETHUSDT"]);

    let tail = assembler.flush().unwrap();
    assert_eq!(bar_symbols(&tail), vec!["SOLUSDT"]);
}

#[test]
fn idle_flush_only_emits_completed_frontiers() {
    let mut assembler = LiveSliceAssembler::new();
    let frontier = DateTime::EPOCH + TimeSpan::from_hours(1);

    assert!(assembler
        .push(LiveDataItem::TradeBar(bar(
            "BTCUSDT",
            frontier - TimeSpan::ONE_MINUTE,
            101
        )))
        .is_empty());
    assert!(assembler
        .flush_ready(frontier - TimeSpan::from_secs(1))
        .is_none());

    let ready = assembler.flush_ready(frontier).unwrap();
    assert_eq!(ready.time, frontier);
    assert_eq!(bar_symbols(&ready), vec!["BTCUSDT"]);
}
