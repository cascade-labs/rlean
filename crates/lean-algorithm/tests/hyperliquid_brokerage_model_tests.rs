use lean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
use lean_core::{Market, Resolution, Symbol};
use lean_orders::OrderStatus;
use rust_decimal_macros::dec;

fn hyperliquid_algorithm() -> (QcAlgorithm, Symbol) {
    let mut algorithm = QcAlgorithm::new("hyperliquid-model-test", dec!(100000));
    algorithm.set_brokerage_model(BrokerageName::HyperliquidBrokerage, AccountType::Margin);
    let symbol = Symbol::create_crypto_future("BTC", &Market::hyperliquid());
    algorithm.register_security_leverage(&symbol, 10.0);
    let symbol = algorithm.add_crypto_future("BTC", &Market::hyperliquid(), Resolution::Minute);
    (algorithm, symbol)
}

#[test]
fn hyperliquid_accepts_passive_post_only_buy_limit() {
    let (mut algorithm, symbol) = hyperliquid_algorithm();
    algorithm
        .securities
        .update_quote(&symbol, dec!(99_900), dec!(100_000));

    let ticket =
        algorithm.limit_order_with_properties(&symbol, dec!(1), dec!(99_950), None, false, true);

    assert_eq!(ticket.status(), OrderStatus::New);
    assert_eq!(algorithm.transactions.get_open_orders().len(), 1);
}

#[test]
fn hyperliquid_rejects_marketable_post_only_buy_limit() {
    let (mut algorithm, symbol) = hyperliquid_algorithm();
    algorithm
        .securities
        .update_quote(&symbol, dec!(99_900), dec!(100_000));

    let ticket =
        algorithm.limit_order_with_properties(&symbol, dec!(1), dec!(100_000), None, false, true);

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(algorithm.transactions.get_open_orders().is_empty());
    assert!(ticket.order_events()[0].message.contains("post-only"));
}

#[test]
fn hyperliquid_rejects_marketable_post_only_sell_limit() {
    let (mut algorithm, symbol) = hyperliquid_algorithm();
    algorithm
        .securities
        .update_quote(&symbol, dec!(99_900), dec!(100_000));

    let ticket =
        algorithm.limit_order_with_properties(&symbol, dec!(-1), dec!(99_900), None, false, true);

    assert_eq!(ticket.status(), OrderStatus::Invalid);
    assert!(algorithm.transactions.get_open_orders().is_empty());
    assert!(ticket.order_events()[0].message.contains("post-only"));
}

#[test]
fn hyperliquid_defers_post_only_cross_check_without_quote() {
    let (mut algorithm, symbol) = hyperliquid_algorithm();
    algorithm.securities.update_price(&symbol, dec!(100_000));

    let ticket =
        algorithm.limit_order_with_properties(&symbol, dec!(1), dec!(100_000), None, false, true);

    assert_eq!(ticket.status(), OrderStatus::New);
    assert_eq!(algorithm.transactions.get_open_orders().len(), 1);
}
