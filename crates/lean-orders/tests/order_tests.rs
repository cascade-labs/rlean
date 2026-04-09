use lean_orders::{Order, OrderType, OrderStatus, OrderDirection, TimeInForce};
use lean_core::{Market, NanosecondTimestamp, Symbol};
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp { NanosecondTimestamp::from_secs(i * 86400) }
fn spy() -> Symbol { Symbol::create_equity("SPY", &Market::usa()) }

// ─── Market order ─────────────────────────────────────────────────────────────

#[test]
fn market_order_fields() {
    let o = Order::market(1, spy(), dec!(100), ts(0), "test");
    assert_eq!(o.id, 1);
    assert_eq!(o.order_type, OrderType::Market);
    assert_eq!(o.quantity, dec!(100));
    assert_eq!(o.status, OrderStatus::New);
    assert!(o.limit_price.is_none());
    assert!(o.stop_price.is_none());
}

#[test]
fn market_order_direction_buy() {
    let o = Order::market(1, spy(), dec!(50), ts(0), "");
    assert_eq!(o.direction(), OrderDirection::Buy);
}

#[test]
fn market_order_direction_sell() {
    let o = Order::market(1, spy(), dec!(-50), ts(0), "");
    assert_eq!(o.direction(), OrderDirection::Sell);
}

#[test]
fn market_order_zero_quantity_hold() {
    let o = Order::market(1, spy(), dec!(0), ts(0), "");
    assert_eq!(o.direction(), OrderDirection::Hold);
}

// ─── Limit order ─────────────────────────────────────────────────────────────

#[test]
fn limit_order_fields() {
    let o = Order::limit(1, spy(), dec!(100), dec!(150), ts(0), "");
    assert_eq!(o.order_type, OrderType::Limit);
    assert_eq!(o.limit_price, Some(dec!(150)));
    assert_eq!(o.price, dec!(150));
}

// ─── Stop market order ───────────────────────────────────────────────────────

#[test]
fn stop_market_order_fields() {
    let o = Order::stop_market(1, spy(), dec!(-100), dec!(90), ts(0), "");
    assert_eq!(o.order_type, OrderType::StopMarket);
    assert_eq!(o.stop_price, Some(dec!(90)));
}

// ─── Stop limit order ────────────────────────────────────────────────────────

#[test]
fn stop_limit_order_fields() {
    let o = Order::stop_limit(1, spy(), dec!(100), dec!(110), dec!(112), ts(0), "");
    assert_eq!(o.order_type, OrderType::StopLimit);
    assert_eq!(o.stop_price, Some(dec!(110)));
    assert_eq!(o.limit_price, Some(dec!(112)));
}

// ─── Order status ────────────────────────────────────────────────────────────

#[test]
fn new_order_is_open() {
    let o = Order::market(1, spy(), dec!(100), ts(0), "");
    assert!(o.is_open());
    assert!(!o.is_filled());
}

#[test]
fn filled_status_is_closed() {
    assert!(OrderStatus::Filled.is_closed());
    assert!(!OrderStatus::Filled.is_open());
}

#[test]
fn submitted_status_is_open() {
    assert!(OrderStatus::Submitted.is_open());
    assert!(!OrderStatus::Submitted.is_closed());
}

#[test]
fn canceled_status_is_closed() {
    assert!(OrderStatus::Canceled.is_closed());
}

// ─── Quantity helpers ────────────────────────────────────────────────────────

#[test]
fn abs_quantity() {
    let o = Order::market(1, spy(), dec!(-200), ts(0), "");
    assert_eq!(o.abs_quantity(), dec!(200));
}

#[test]
fn remaining_quantity_when_unfilled() {
    let o = Order::market(1, spy(), dec!(100), ts(0), "");
    assert_eq!(o.remaining_quantity(), dec!(100));
}

// ─── TimeInForce ─────────────────────────────────────────────────────────────

#[test]
fn default_time_in_force_is_gtc() {
    let o = Order::market(1, spy(), dec!(100), ts(0), "");
    assert_eq!(o.time_in_force, TimeInForce::GoodTilCanceled);
}

// ─── Order direction opposite ────────────────────────────────────────────────

#[test]
fn direction_opposite() {
    assert_eq!(OrderDirection::Buy.opposite(), OrderDirection::Sell);
    assert_eq!(OrderDirection::Sell.opposite(), OrderDirection::Buy);
    assert_eq!(OrderDirection::Hold.opposite(), OrderDirection::Hold);
}
