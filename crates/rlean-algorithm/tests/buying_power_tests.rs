use rlean_algorithm::qc_algorithm::QcAlgorithm;
use rlean_core::{DateTime, Market, Resolution, Symbol};
use rlean_orders::Order;
use rust_decimal_macros::dec;

#[test]
#[should_panic(expected = "requires maxLeverage metadata")]
fn hyperliquid_crypto_future_requires_registered_leverage_metadata() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));

    algorithm.add_crypto_future("BTC", &Market::hyperliquid(), Resolution::Minute);
}

#[test]
fn set_holdings_targets_crypto_future_portfolio_weight() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let symbol = Symbol::create_crypto_future("BTC", &Market::hyperliquid());
    algorithm.register_security_leverage(&symbol, 10.0);
    let symbol = algorithm.add_crypto_future("BTC", &Market::hyperliquid(), Resolution::Minute);
    algorithm.securities.update_price(&symbol, dec!(100));

    algorithm.set_holdings(&symbol, dec!(1));

    let order = algorithm.transactions.get_all_orders().pop().unwrap();
    assert_eq!(order.quantity, dec!(1000));
}

#[test]
fn crypto_future_buying_power_rejects_over_margin_order() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let symbol = Symbol::create_crypto_future("BTC", &Market::hyperliquid());
    algorithm.register_security_leverage(&symbol, 10.0);
    let symbol = algorithm.add_crypto_future("BTC", &Market::hyperliquid(), Resolution::Minute);
    algorithm.securities.update_price(&symbol, dec!(100));

    let valid = Order::market(1, symbol.clone(), dec!(10000), DateTime::EPOCH, "");
    assert!(algorithm
        .validate_order_buying_power(&valid, dec!(100), dec!(0))
        .is_ok());

    let invalid = Order::market(2, symbol, dec!(10001), DateTime::EPOCH, "");
    let error = algorithm
        .validate_order_buying_power(&invalid, dec!(100), dec!(0))
        .unwrap_err();
    assert!(error.contains("Insufficient buying power"));
}
