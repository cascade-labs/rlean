use rlean_algorithm::portfolio::SecurityPortfolioManager;
use rlean_brokerages::{Brokerage, PaperBrokerage};
use rlean_core::{DateTime, Market, Symbol, TimeSpan};
use rlean_data::Slice;
use rlean_data_tables::{TradeBar, TradeBarData};
use rlean_orders::{Order, OrderStatus, TransactionManager};
use rust_decimal_macros::dec;
use std::sync::Arc;

fn symbol() -> Symbol {
    Symbol::create_crypto("BTCUSDT", &Market::binance())
}

fn market_bar(symbol: Symbol, time: DateTime) -> TradeBar {
    TradeBar::new(
        symbol,
        time,
        TimeSpan::ONE_MINUTE,
        TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(10)),
    )
}

#[test]
fn place_order_records_submitted_event_and_shared_transaction_state() {
    let portfolio = Arc::new(SecurityPortfolioManager::new(dec!(10000)));
    let transactions = Arc::new(TransactionManager::new());
    let mut brokerage =
        PaperBrokerage::new_with_transactions(dec!(10000), portfolio, transactions.clone());
    let order = Order::market(1, symbol(), dec!(10), DateTime::EPOCH, "paper");

    assert!(brokerage.place_order(order.clone()).unwrap());

    assert_eq!(
        transactions.get_order(order.id).unwrap().status,
        OrderStatus::Submitted
    );
    assert_eq!(brokerage.order_events().len(), 1);
    assert_eq!(brokerage.order_events()[0].status, OrderStatus::Submitted);
}

#[test]
fn limit_order_submitted_event_preserves_limit_price() {
    let portfolio = Arc::new(SecurityPortfolioManager::new(dec!(10000)));
    let transactions = Arc::new(TransactionManager::new());
    let mut brokerage =
        PaperBrokerage::new_with_transactions(dec!(10000), portfolio, transactions.clone());
    let order = Order::limit(1, symbol(), dec!(10), dec!(99), DateTime::EPOCH, "paper");

    assert!(brokerage.place_order(order).unwrap());

    assert_eq!(brokerage.order_events()[0].status, OrderStatus::Submitted);
    assert_eq!(brokerage.order_events()[0].limit_price, Some(dec!(99)));
}

#[test]
fn scan_fills_market_order_and_settles_portfolio() {
    let portfolio = Arc::new(SecurityPortfolioManager::new(dec!(10000)));
    let transactions = Arc::new(TransactionManager::new());
    let mut brokerage =
        PaperBrokerage::new_with_transactions(dec!(10000), portfolio.clone(), transactions.clone());
    let symbol = symbol();
    let order = Order::market(1, symbol.clone(), dec!(10), DateTime::EPOCH, "paper");
    brokerage.place_order(order.clone()).unwrap();

    let bar = market_bar(symbol.clone(), DateTime::EPOCH);
    let mut slice = Slice::new(bar.end_time);
    slice.add_bar(bar);

    let events = brokerage.scan(&slice).unwrap();

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].status, OrderStatus::Filled);
    assert_eq!(events[0].fill_price, dec!(100));
    assert_eq!(
        transactions.get_order(order.id).unwrap().status,
        OrderStatus::Filled
    );
    assert_eq!(portfolio.get_holding(&symbol).quantity, dec!(10));
    assert_eq!(
        brokerage.get_cash_balance(),
        vec![("USD".to_string(), dec!(9000))]
    );
}
