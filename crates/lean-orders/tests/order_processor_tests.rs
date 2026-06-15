use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::{Bar, QuoteBar, TradeBar, TradeBarData};
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    Order, TransactionManager,
};
use rust_decimal_macros::dec;
use std::{collections::HashMap, sync::Arc};

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i)
}

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn trade_bar(symbol: Symbol) -> TradeBar {
    TradeBar::new(
        symbol,
        ts(0),
        TimeSpan::ONE_MINUTE,
        TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
    )
}

fn quote_bar(symbol: Symbol) -> QuoteBar {
    QuoteBar::new(
        symbol,
        ts(0),
        TimeSpan::ONE_MINUTE,
        Some(Bar::new(dec!(99), dec!(100), dec!(98), dec!(99))),
        Some(Bar::new(dec!(101), dec!(102), dec!(99), dec!(101))),
        dec!(100),
        dec!(100),
    )
}

fn passive_quote_bar(symbol: Symbol) -> QuoteBar {
    QuoteBar::new(
        symbol,
        ts(60),
        TimeSpan::ONE_MINUTE,
        Some(Bar::new(dec!(99), dec!(100), dec!(98), dec!(99))),
        Some(Bar::new(dec!(101), dec!(102), dec!(100), dec!(101))),
        dec!(100),
        dec!(100),
    )
}

#[test]
fn process_orders_with_quotes_uses_quote_aware_immediate_market_fill() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    tm.add_order(Order::market(1, symbol.clone(), dec!(10), ts(0), ""));
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(101));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn process_orders_with_quotes_uses_quote_side_for_limit_trigger() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    tm.add_order(Order::limit(
        1,
        symbol.clone(),
        dec!(10),
        dec!(100),
        ts(0),
        "",
    ));
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(100));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn process_orders_with_quotes_does_not_fill_limit_on_submission_slice() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    tm.add_order(Order::limit(
        1,
        symbol.clone(),
        dec!(10),
        dec!(100),
        ts(60),
        "",
    ));
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert!(events.is_empty());
}

#[test]
fn post_only_buy_limit_uses_same_side_bid_for_passive_fill() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    let mut order = Order::limit(1, symbol.clone(), dec!(10), dec!(99.5), ts(0), "");
    order.properties.post_only = true;
    tm.add_order(order);
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, passive_quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(99.5));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn post_only_sell_limit_uses_same_side_ask_for_passive_fill() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    let mut order = Order::limit(1, symbol.clone(), dec!(-10), dec!(100.5), ts(0), "");
    order.properties.post_only = true;
    tm.add_order(order);
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, passive_quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(100.5));
    assert_eq!(events[0].fill_quantity, dec!(-10));
}
