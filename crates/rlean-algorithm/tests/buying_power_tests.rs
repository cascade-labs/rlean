use rlean_algorithm::buying_power::BuyingPowerModel;
use rlean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
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

#[test]
fn robinhood_cash_model_initializes_equities_with_one_times_buying_power() {
    let mut algorithm = QcAlgorithm::new("test", dec!(100000));
    algorithm.set_brokerage_model(BrokerageName::RobinhoodBrokerage, AccountType::Cash);
    let symbol = algorithm.add_equity("SPY", Resolution::Minute);
    let security = algorithm.securities.get(&symbol).unwrap();

    assert_eq!(
        algorithm.brokerage_model.brokerage,
        BrokerageName::RobinhoodBrokerage
    );
    assert_eq!(algorithm.brokerage_model.account_type, AccountType::Cash);
    assert_eq!(security.buying_power_model(), BuyingPowerModel::Cash);
    assert_eq!(security.leverage(), 1.0);
}

#[test]
fn cash_buying_power_does_not_spend_unfilled_reduction_proceeds() {
    let mut algorithm = QcAlgorithm::new("test", dec!(8226));
    algorithm.set_brokerage_model(BrokerageName::RobinhoodBrokerage, AccountType::Cash);

    for (index, ticker) in ["STIM", "EOSE", "RIOT", "AA"].into_iter().enumerate() {
        let symbol = algorithm.add_equity(ticker, Resolution::Minute);
        algorithm.securities.update_price(&symbol, dec!(100));
        algorithm
            .portfolio
            .set_holdings(&symbol, dec!(100), dec!(200), dec!(1));
        // These reductions are merely working orders. Like C# LEAN, their
        // anticipated proceeds are not buying power until their fills settle.
        algorithm.transactions.add_order(Order::market(
            index as i64 + 1,
            symbol,
            dec!(-30),
            DateTime::EPOCH,
            "rebalance reduction",
        ));
    }

    let replacement = algorithm.add_equity("FHN", Resolution::Minute);
    algorithm.securities.update_price(&replacement, dec!(25.39));
    let buy = Order::market(10, replacement, dec!(419), DateTime::EPOCH, "replacement");

    let error = algorithm
        .validate_order_submission_buying_power(&buy)
        .unwrap_err();
    assert!(error.contains("Insufficient buying power"));
    assert!(error.contains("FHN"));
}
