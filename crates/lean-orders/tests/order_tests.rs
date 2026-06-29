use lean_core::{Market, NanosecondTimestamp, Symbol};
use lean_orders::{
    transaction_manager::TransactionManager, Order, OrderDirection, OrderEvent, OrderStatus,
    OrderType, TimeInForce, UpdateOrderFields,
};
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}
fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

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

#[test]
fn transaction_manager_apply_split_updates_open_orders() {
    let tm = TransactionManager::new();
    let symbol = spy();
    let order = Order::stop_limit(1, symbol.clone(), dec!(100), dec!(90), dec!(95), ts(0), "");
    tm.add_order(order);

    tm.apply_split_to_open_orders(symbol.id.sid, dec!(0.5));

    let adjusted = tm.get_order(1).expect("order should exist");
    assert_eq!(adjusted.quantity, dec!(200));
    assert_eq!(adjusted.stop_price, Some(dec!(45.0)));
    assert_eq!(adjusted.limit_price, Some(dec!(47.5)));
}

#[test]
fn transaction_manager_cancel_open_orders_updates_canonical_order() {
    let tm = TransactionManager::new();
    let symbol = spy();
    let ticket = tm.add_order(Order::market(1, symbol, dec!(100), ts(0), ""));

    tm.cancel_open_orders(ts(1));

    let order = tm.get_order(1).expect("order should exist");
    assert_eq!(order.status, OrderStatus::Canceled);
    assert_eq!(order.canceled_time, Some(ts(1)));
    assert_eq!(ticket.status(), OrderStatus::Canceled);
    assert!(tm.get_open_orders().is_empty());
}

#[test]
fn transaction_manager_request_cancel_sets_cancel_pending_and_tracks_request() {
    let tm = TransactionManager::new();
    let symbol = spy();
    let ticket = tm.add_order(Order::market(1, symbol.clone(), dec!(100), ts(0), ""));

    assert!(tm.request_cancel_order(1, ts(1), "cancel".to_string()));

    let order = tm.get_order(1).expect("order should exist");
    assert_eq!(order.status, OrderStatus::CancelPending);
    assert_eq!(ticket.status(), OrderStatus::CancelPending);
    assert!(tm.get_open_orders().is_empty());
    let requests = tm.get_cancel_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].order_id, 1);
    assert_eq!(requests[0].tag, "cancel");
}

#[test]
fn transaction_manager_update_new_order_applies_without_queueing_request() {
    let tm = TransactionManager::new();
    let symbol = spy();
    let ticket = tm.add_order(Order::limit(1, symbol, dec!(100), dec!(450), ts(0), ""));

    assert!(tm.request_update_order(
        1,
        ts(1),
        UpdateOrderFields {
            limit_price: Some(dec!(451)),
            tag: Some("adjusted".to_string()),
            ..Default::default()
        },
    ));

    let order = tm.get_order(1).expect("order should exist");
    assert_eq!(order.status, OrderStatus::New);
    assert_eq!(order.limit_price, Some(dec!(451)));
    assert_eq!(order.tag, "adjusted");
    assert_eq!(ticket.order().limit_price, Some(dec!(451)));
    assert!(tm.get_update_requests().is_empty());
}

#[test]
fn transaction_manager_update_submitted_order_sets_update_submitted_and_tracks_request() {
    let tm = TransactionManager::new();
    let symbol = spy();
    let mut order = Order::limit(1, symbol, dec!(100), dec!(450), ts(0), "");
    order.status = OrderStatus::Submitted;
    let ticket = tm.add_order(order);

    assert!(tm.request_update_order(
        1,
        ts(1),
        UpdateOrderFields {
            limit_price: Some(dec!(451)),
            ..Default::default()
        },
    ));

    let order = tm.get_order(1).expect("order should exist");
    assert_eq!(order.status, OrderStatus::UpdateSubmitted);
    assert_eq!(order.limit_price, Some(dec!(451)));
    assert_eq!(ticket.status(), OrderStatus::UpdateSubmitted);
    let requests = tm.get_update_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].order_id, 1);
    assert_eq!(requests[0].previous_order.limit_price, Some(dec!(450)));
}

#[test]
fn transaction_manager_cancel_open_orders_for_symbol_leaves_other_symbols_open() {
    let tm = TransactionManager::new();
    let spy = spy();
    let aapl = Symbol::create_equity("AAPL", &Market::usa());
    tm.add_order(Order::market(1, spy.clone(), dec!(100), ts(0), ""));
    tm.add_order(Order::market(2, aapl.clone(), dec!(100), ts(0), ""));

    tm.cancel_open_orders_for_symbol(spy.id.sid, ts(1));

    assert_eq!(tm.get_order(1).unwrap().status, OrderStatus::Canceled);
    assert_eq!(tm.get_order(2).unwrap().status, OrderStatus::New);
    let open = tm.get_open_orders();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].symbol, aapl);
}

#[test]
fn transaction_manager_tracks_open_order_index() {
    let tm = TransactionManager::new();
    let symbol = spy();
    assert!(!tm.has_open_orders());

    tm.add_order(Order::market(1, symbol.clone(), dec!(100), ts(0), ""));
    assert!(tm.has_open_orders());
    assert_eq!(tm.get_open_orders().len(), 1);

    tm.process_order_event(OrderEvent::filled(
        1,
        symbol.clone(),
        ts(1),
        dec!(450),
        dec!(100),
    ));
    assert!(!tm.has_open_orders());
    assert!(tm.get_open_orders().is_empty());
}
