use chrono::TimeZone;
use lean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
use lean_core::{Market, OptionRight, OptionStyle, Resolution, Symbol, SymbolOptionsExt};
use lean_orders::{Order, OrderStatus, TimeInForce};
use rust_decimal_macros::dec;

fn tradier_algorithm() -> QcAlgorithm {
    let mut algorithm = QcAlgorithm::new("tradier-model-test", dec!(100000));
    algorithm.set_brokerage_model(BrokerageName::TradierBrokerage, AccountType::Margin);
    algorithm
}

#[test]
fn tradier_accepts_supported_equity_buy_orders() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);

    let ticket = algorithm.market_order(&symbol, dec!(1));

    assert_eq!(ticket.status(), OrderStatus::New);
    assert_eq!(algorithm.transactions.get_open_orders().len(), 1);
}

#[test]
fn tradier_rejects_unsupported_market_on_open_orders() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);

    let ticket = algorithm.market_on_open_order(&symbol, dec!(1));

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(algorithm.transactions.get_open_orders().is_empty());
    assert!(ticket.order_events()[0]
        .message
        .contains("does not support MarketOnOpen"));
}

#[test]
fn tradier_rejects_unsupported_security_types() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_forex("EURUSD", Resolution::Minute);

    let ticket = algorithm.market_order(&symbol, dec!(1000));

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(algorithm.transactions.get_open_orders().is_empty());
    assert!(ticket.order_events()[0]
        .message
        .contains("does not support Forex"));
}

#[test]
fn tradier_rejects_gtc_orders_that_leave_short_position() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);

    let ticket = algorithm.market_order(&symbol, dec!(-1));

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(algorithm.transactions.get_open_orders().is_empty());
    assert!(ticket.order_events()[0].message.contains("GTC"));
}

#[test]
fn tradier_accepts_day_short_orders_when_price_is_above_five() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    algorithm.securities.update_price(&symbol, dec!(100));

    let ticket =
        algorithm.market_order_with_time_in_force(&symbol, dec!(-1), Some(TimeInForce::Day));

    assert_eq!(ticket.status(), OrderStatus::New);
    assert_eq!(algorithm.transactions.get_open_orders().len(), 1);
}

#[test]
fn tradier_accepts_extended_hours_equity_limit_order_during_pre_market() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    algorithm.utc_time = ny_time(2026, 1, 16, 8, 0);

    let ticket = algorithm.limit_order_with_options(
        &symbol,
        dec!(1),
        dec!(450),
        Some(TimeInForce::Day),
        true,
    );

    assert_eq!(ticket.status(), OrderStatus::New);
    assert!(ticket.order().properties.outside_regular_trading_hours);
}

#[test]
fn tradier_paper_fills_wait_for_regular_session_without_extended_hours_flag() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    algorithm.utc_time = ny_time(2026, 1, 16, 8, 0);

    let ticket = algorithm.limit_order_with_options(
        &symbol,
        dec!(1),
        dec!(450),
        Some(TimeInForce::Day),
        false,
    );

    assert_eq!(ticket.status(), OrderStatus::New);
    assert!(!algorithm.can_execute_order_with_brokerage_model(&ticket.order()));

    algorithm.utc_time = ny_time(2026, 1, 16, 10, 0);
    assert!(algorithm.can_execute_order_with_brokerage_model(&ticket.order()));
}

#[test]
fn tradier_paper_fills_execute_extended_hours_equity_limit_during_pre_market() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    algorithm.utc_time = ny_time(2026, 1, 16, 8, 0);

    let ticket = algorithm.limit_order_with_options(
        &symbol,
        dec!(1),
        dec!(450),
        Some(TimeInForce::Day),
        true,
    );

    assert_eq!(ticket.status(), OrderStatus::New);
    assert!(algorithm.can_execute_order_with_brokerage_model(&ticket.order()));
}

#[test]
fn tradier_rejects_extended_hours_non_limit_orders() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    algorithm.utc_time = ny_time(2026, 1, 16, 8, 0);

    let ticket =
        algorithm.market_order_with_options(&symbol, dec!(1), Some(TimeInForce::Day), true);

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(ticket.order_events()[0].message.contains("extended-hours"));
}

#[test]
fn tradier_rejects_extended_hours_order_outside_extended_session() {
    let mut algorithm = tradier_algorithm();
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    algorithm.utc_time = ny_time(2026, 1, 16, 20, 0);

    let ticket = algorithm.limit_order_with_options(
        &symbol,
        dec!(1),
        dec!(450),
        Some(TimeInForce::Day),
        true,
    );

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(ticket.order_events()[0].message.contains("extended-hours"));
}

#[test]
fn tradier_accepts_supported_index_option_orders() {
    let mut algorithm = tradier_algorithm();
    let underlying = Symbol::create_index("SPX", &Market::usa());
    let index_option = Symbol::create_index_option_osi(
        underlying,
        dec!(4500),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
        OptionRight::Call,
        OptionStyle::European,
        &Market::usa(),
    );

    let ticket = algorithm.limit_order(&index_option, dec!(1), dec!(12.50));

    assert_eq!(
        index_option.security_type(),
        lean_core::SecurityType::IndexOption
    );
    assert_eq!(ticket.status(), OrderStatus::New);
}

#[test]
fn tradier_algorithm_uses_zero_fee_model_for_options() {
    let algorithm = tradier_algorithm();
    let underlying = Symbol::create_equity("SPY", &Market::usa());
    let option = Symbol::create_option(
        underlying,
        &Market::usa(),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
        dec!(450),
        OptionRight::Call,
        OptionStyle::American,
    );
    let order = Order::market(1, option, dec!(10), lean_core::DateTime::EPOCH, "");

    let fee = algorithm.order_fee(&order, dec!(1.25));

    assert_eq!(fee.amount, dec!(0));
}

fn ny_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> lean_core::DateTime {
    let local = lean_core::time::tz::NEW_YORK
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .unwrap();
    local.with_timezone(&chrono::Utc).into()
}
