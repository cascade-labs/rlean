use rlean_core::{DataNormalizationMode, DateTime, Market, Resolution, Symbol, TickType, TimeSpan};
use rlean_data::{LiveDataItem, Slice, SubscriptionDataConfig};
use rlean_data_tables::{Bar, CustomDataPoint, QuoteBar, TradeBar, TradeBarData};
use rlean_live::LiveSliceAssembler;
use rust_decimal_macros::dec;
use std::collections::HashMap;

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

fn utc(value: &str) -> DateTime {
    DateTime::from(
        value
            .parse::<chrono::DateTime<chrono::Utc>>()
            .expect("valid UTC timestamp"),
    )
}

fn spy_quote(time: DateTime, bid: i64, ask: i64) -> QuoteBar {
    QuoteBar::new(
        Symbol::create_equity("SPY", &Market::usa()),
        time,
        TimeSpan::ONE_MINUTE,
        Some(Bar::from_price(bid.into())),
        Some(Bar::from_price(ask.into())),
        dec!(100),
        dec!(100),
    )
}

fn custom_item(time: DateTime) -> LiveDataItem {
    LiveDataItem::CustomData {
        symbol: Symbol::create_base("unusual_whales", "market_tide", &Market::usa()),
        source_type: "unusual_whales".into(),
        ticker: "market_tide".into(),
        point: CustomDataPoint::new(time, time, dec!(1), HashMap::new()),
    }
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

#[test]
fn custom_data_slice_contains_fill_forward_quote_from_active_subscription() {
    let spy = Symbol::create_equity("SPY", &Market::usa());
    let mut quote_config = SubscriptionDataConfig::new_equity(
        spy.clone(),
        Resolution::Minute,
        DataNormalizationMode::Raw,
    );
    quote_config.set_tick_type(TickType::Quote);

    let mut assembler = LiveSliceAssembler::new();
    assembler.set_subscriptions([&quote_config]);

    let quote_time = utc("2026-07-28T15:59:00Z");
    assembler.enqueue(LiveDataItem::QuoteBar(spy_quote(quote_time, 635, 636)));
    let first = assembler.advance(utc("2026-07-28T16:00:00Z")).unwrap();
    assert!(first.quote_bars.contains_key(&spy.id.sid));

    let custom_time = utc("2026-07-28T16:05:00Z");
    assembler.enqueue(custom_item(custom_time));
    let slice = assembler.advance(custom_time).unwrap();

    assert_eq!(slice.custom_data["MARKET_TIDE"].len(), 1);
    let quote = &slice.quote_bars[&spy.id.sid];
    assert_eq!(quote.time, utc("2026-07-28T16:04:00Z"));
    assert_eq!(quote.end_time, custom_time);
    assert_eq!(quote.bid.as_ref().unwrap().close, dec!(635));
    assert_eq!(quote.ask.as_ref().unwrap().close, dec!(636));
}

#[test]
fn real_quote_at_custom_frontier_wins_over_fill_forward() {
    let spy = Symbol::create_equity("SPY", &Market::usa());
    let mut quote_config = SubscriptionDataConfig::new_equity(
        spy.clone(),
        Resolution::Minute,
        DataNormalizationMode::Raw,
    );
    quote_config.set_tick_type(TickType::Quote);

    let mut assembler = LiveSliceAssembler::new();
    assembler.set_subscriptions([&quote_config]);
    assembler.enqueue(LiveDataItem::QuoteBar(spy_quote(
        utc("2026-07-28T15:59:00Z"),
        635,
        636,
    )));
    assembler.advance(utc("2026-07-28T16:00:00Z")).unwrap();

    let custom_time = utc("2026-07-28T16:05:00Z");
    assembler.enqueue(LiveDataItem::QuoteBar(spy_quote(
        utc("2026-07-28T16:04:00Z"),
        640,
        641,
    )));
    assembler.enqueue(custom_item(custom_time));
    let slice = assembler.advance(custom_time).unwrap();

    let quote = &slice.quote_bars[&spy.id.sid];
    assert_eq!(quote.bid.as_ref().unwrap().close, dec!(640));
    assert_eq!(quote.ask.as_ref().unwrap().close, dec!(641));
}

#[test]
fn removed_subscription_does_not_fill_forward() {
    let spy = Symbol::create_equity("SPY", &Market::usa());
    let mut quote_config = SubscriptionDataConfig::new_equity(
        spy.clone(),
        Resolution::Minute,
        DataNormalizationMode::Raw,
    );
    quote_config.set_tick_type(TickType::Quote);

    let mut assembler = LiveSliceAssembler::new();
    assembler.set_subscriptions([&quote_config]);
    assembler.enqueue(LiveDataItem::QuoteBar(spy_quote(
        utc("2026-07-28T15:59:00Z"),
        635,
        636,
    )));
    assembler.advance(utc("2026-07-28T16:00:00Z")).unwrap();
    assembler.set_subscriptions(std::iter::empty());

    let custom_time = utc("2026-07-28T16:05:00Z");
    assembler.enqueue(custom_item(custom_time));
    let slice = assembler.advance(custom_time).unwrap();
    assert!(!slice.quote_bars.contains_key(&spy.id.sid));
}
