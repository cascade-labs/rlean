use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::{Bar, QuoteBar, TradeBar, TradeBarData};
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    Order, OrderType, TransactionManager,
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

fn same_side_only_quote_bar(symbol: Symbol) -> QuoteBar {
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

fn buy_fill_quote_bar(symbol: Symbol) -> QuoteBar {
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

fn sell_fill_quote_bar(symbol: Symbol) -> QuoteBar {
    QuoteBar::new(
        symbol,
        ts(60),
        TimeSpan::ONE_MINUTE,
        Some(Bar::new(dec!(99), dec!(101), dec!(98), dec!(99))),
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
    assert_eq!(events[0].fill_price, dec!(101));
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
fn post_algorithm_scan_fills_new_market_order_on_current_slice() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    tm.add_order(Order::market(1, symbol.clone(), dec!(10), ts(60), ""));
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.generate_post_algorithm_order_events_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(101));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn post_algorithm_scan_defers_new_limit_order_on_current_slice() {
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

    let events = processor.generate_post_algorithm_order_events_with_quotes(&bars, &quotes, ts(60));

    assert!(events.is_empty());
}

#[test]
fn post_algorithm_scan_defers_new_market_on_open_order_on_current_slice() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    let mut order = Order::market(1, symbol.clone(), dec!(10), ts(60), "");
    order.order_type = OrderType::MarketOnOpen;
    tm.add_order(order);
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.generate_post_algorithm_order_events_with_quotes(&bars, &quotes, ts(60));

    assert!(events.is_empty());
}

#[test]
fn post_algorithm_scan_fills_resting_market_on_open_order_from_prior_slice() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    let mut order = Order::market(1, symbol.clone(), dec!(10), ts(0), "");
    order.order_type = OrderType::MarketOnOpen;
    tm.add_order(order);
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.generate_post_algorithm_order_events_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(101));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn post_algorithm_scan_fills_resting_limit_order_from_prior_slice() {
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

    let events = processor.generate_post_algorithm_order_events_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(100));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn post_algorithm_scan_defers_limit_order_updated_on_current_slice() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    let mut order = Order::limit(1, symbol.clone(), dec!(10), dec!(100), ts(0), "");
    order.last_update_time = Some(ts(60));
    tm.add_order(order);
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, quote_bar(symbol.clone()))]);

    let events = processor.generate_post_algorithm_order_events_with_quotes(&bars, &quotes, ts(60));

    assert!(events.is_empty());
}

#[test]
fn post_only_buy_limit_does_not_fill_from_same_side_bid_penetration() {
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
    let quotes = HashMap::from([(symbol.id.sid, same_side_only_quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert!(events.is_empty());
}

#[test]
fn post_only_buy_limit_uses_ask_quote_like_lean_limit_fill() {
    let symbol = spy();
    let tm = Arc::new(TransactionManager::new());
    let mut order = Order::limit(1, symbol.clone(), dec!(10), dec!(100.5), ts(0), "");
    order.properties.post_only = true;
    tm.add_order(order);
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        tm,
    );

    let bars = HashMap::from([(symbol.id.sid, trade_bar(symbol.clone()))]);
    let quotes = HashMap::from([(symbol.id.sid, buy_fill_quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(100.5));
    assert_eq!(events[0].fill_quantity, dec!(10));
}

#[test]
fn post_only_sell_limit_does_not_fill_from_same_side_ask_penetration() {
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
    let quotes = HashMap::from([(symbol.id.sid, same_side_only_quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert!(events.is_empty());
}

#[test]
fn post_only_sell_limit_uses_bid_quote_like_lean_limit_fill() {
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
    let quotes = HashMap::from([(symbol.id.sid, sell_fill_quote_bar(symbol.clone()))]);

    let events = processor.process_orders_with_quotes(&bars, &quotes, ts(60));

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].fill_price, dec!(100.5));
    assert_eq!(events[0].fill_quantity, dec!(-10));
}
