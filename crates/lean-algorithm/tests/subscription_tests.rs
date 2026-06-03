use lean_algorithm::qc_algorithm::{BrokerageName, QcAlgorithm};
use lean_core::{Market, Resolution, SecurityType, TickType};
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

#[test]
fn add_crypto_future_adds_trade_and_quote_subscriptions() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));

    let symbol = algorithm.add_crypto_future("BTC", &Market::hyperliquid(), Resolution::Minute);

    assert_eq!(symbol.security_type(), SecurityType::CryptoFuture);
    assert_eq!(symbol.market().as_str(), Market::HYPERLIQUID);
    let subscriptions = algorithm.subscription_manager.get_all();
    assert_eq!(subscriptions.len(), 2);
    assert!(subscriptions
        .iter()
        .any(|sub| sub.symbol == symbol && sub.tick_type == TickType::Trade));
    assert!(subscriptions
        .iter()
        .any(|sub| sub.symbol == symbol && sub.tick_type == TickType::Quote));
    let security = algorithm.securities.get(&symbol).unwrap();
    assert_eq!(security.leverage(), 25.0);
    assert_eq!(security.symbol_properties.quote_currency, "USDC");
}

#[test]
fn hyperliquid_brokerage_defaults_crypto_future_market_to_hyperliquid() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    algorithm.set_brokerage_model(
        BrokerageName::HyperliquidBrokerage,
        lean_algorithm::qc_algorithm::AccountType::Margin,
    );

    let market = algorithm.default_market_for_security(SecurityType::CryptoFuture);

    assert_eq!(market.as_str(), Market::HYPERLIQUID);
}
