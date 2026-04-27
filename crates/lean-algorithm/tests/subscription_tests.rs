use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{Resolution, TickType};
use rust_decimal_macros::dec;

#[test]
fn add_equity_minute_adds_trade_and_quote_subscriptions() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));

    algorithm.add_equity("SPY", Resolution::Minute);

    let subscriptions = algorithm.subscription_manager.get_all();
    assert_eq!(subscriptions.len(), 2);
    assert!(subscriptions
        .iter()
        .any(|sub| sub.symbol.value == "SPY" && sub.tick_type == TickType::Trade));
    assert!(subscriptions
        .iter()
        .any(|sub| sub.symbol.value == "SPY" && sub.tick_type == TickType::Quote));
}

#[test]
fn add_equity_existing_security_still_adds_later_minute_quote_subscription() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));

    algorithm.add_equity("SPY", Resolution::Daily);
    algorithm.add_equity("SPY", Resolution::Minute);

    let subscriptions = algorithm.subscription_manager.get_all();
    assert!(subscriptions
        .iter()
        .any(|sub| sub.symbol.value == "SPY" && sub.resolution == Resolution::Daily));
    assert!(subscriptions.iter().any(|sub| {
        sub.symbol.value == "SPY"
            && sub.resolution == Resolution::Minute
            && sub.tick_type == TickType::Trade
    }));
    assert!(subscriptions.iter().any(|sub| {
        sub.symbol.value == "SPY"
            && sub.resolution == Resolution::Minute
            && sub.tick_type == TickType::Quote
    }));
}
