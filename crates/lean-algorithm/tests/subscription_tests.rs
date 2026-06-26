use lean_algorithm::qc_algorithm::{BrokerageName, QcAlgorithm};
use lean_core::{
    DataNormalizationMode, Market, OptionRight, OptionStyle, Resolution, SecurityType, Symbol,
    TickType,
};
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
    let expected_symbol = Symbol::create_crypto_future("BTC", &Market::hyperliquid());
    algorithm.register_security_leverage(&expected_symbol, 50.0);

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
    assert_eq!(security.leverage(), 50.0);
    assert_eq!(security.symbol_properties.quote_currency, "USDC");
}

#[test]
fn add_option_contract_minute_adds_trade_and_quote_subscriptions() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let underlying = Symbol::create_equity("SPY", &Market::usa());
    let option = Symbol::create_option(
        underlying,
        &Market::usa(),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
        dec!(450),
        OptionRight::Call,
        OptionStyle::American,
    );

    let symbol = algorithm.add_option_contract(option.clone(), Resolution::Minute);

    let subscriptions = algorithm.subscription_manager.get_all();
    assert!(subscriptions.iter().any(|sub| {
        sub.symbol == symbol
            && sub.resolution == Resolution::Minute
            && sub.tick_type == TickType::Trade
    }));
    assert!(subscriptions.iter().any(|sub| {
        sub.symbol == symbol
            && sub.resolution == Resolution::Minute
            && sub.tick_type == TickType::Quote
    }));
}

#[test]
fn add_equity_defaults_to_adjusted_normalization() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    for sub in algorithm
        .subscription_manager
        .get_configs_for_symbol(&symbol)
    {
        assert_eq!(sub.normalization_mode, DataNormalizationMode::Adjusted);
    }
}

#[test]
fn add_equity_with_normalization_stores_requested_mode() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let symbol = algorithm.add_equity_with_normalization(
        "SPY",
        Resolution::Minute,
        Some(DataNormalizationMode::Raw),
    );
    for sub in algorithm
        .subscription_manager
        .get_configs_for_symbol(&symbol)
    {
        assert_eq!(sub.normalization_mode, DataNormalizationMode::Raw);
    }
}

#[test]
fn set_data_normalization_mode_flips_trade_and_quote_configs() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    let updated = algorithm.set_data_normalization_mode(&symbol, DataNormalizationMode::Raw);
    assert_eq!(updated, 2);
    for sub in algorithm
        .subscription_manager
        .get_configs_for_symbol(&symbol)
    {
        assert_eq!(sub.normalization_mode, DataNormalizationMode::Raw);
    }
}

#[test]
fn add_option_contract_subscriptions_are_raw_and_force_underlying_raw() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    let underlying = Symbol::create_equity("SPY", &Market::usa());
    let option = Symbol::create_option(
        underlying.clone(),
        &Market::usa(),
        chrono::NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
        dec!(450),
        OptionRight::Call,
        OptionStyle::American,
    );

    let symbol = algorithm.add_option_contract(option, Resolution::Minute);

    for sub in algorithm
        .subscription_manager
        .get_configs_for_symbol(&symbol)
    {
        assert_eq!(sub.normalization_mode, DataNormalizationMode::Raw);
    }
    let underlying_subs = algorithm
        .subscription_manager
        .get_configs_for_symbol(&underlying);
    assert!(!underlying_subs.is_empty());
    for sub in underlying_subs {
        assert_eq!(sub.normalization_mode, DataNormalizationMode::Raw);
    }
}

#[test]
fn add_option_forces_underlying_to_raw() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    algorithm.add_option("SPY", Resolution::Minute);
    let underlying = Symbol::create_equity("SPY", &Market::usa());
    let subs = algorithm
        .subscription_manager
        .get_configs_for_symbol(&underlying);
    assert!(!subs.is_empty());
    for sub in subs {
        assert_eq!(sub.normalization_mode, DataNormalizationMode::Raw);
    }
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
