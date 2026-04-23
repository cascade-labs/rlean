use chrono::NaiveDate;
use lean_algorithm::QcAlgorithm;
use lean_core::{Market, OptionRight, OptionStyle, Symbol};
use rust_decimal_macros::dec;

fn spy_put() -> Symbol {
    let underlying = Symbol::create_equity("SPY", &Market::usa());
    Symbol::create_option(
        underlying,
        &Market::usa(),
        NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
        dec!(450),
        OptionRight::Put,
        OptionStyle::American,
    )
}

#[test]
fn sell_to_open_credits_cash_and_tracks_short_option_holding() {
    let mut alg = QcAlgorithm::new("test", dec!(100_000));
    let option = spy_put();

    alg.sell_to_open(option.clone(), dec!(2), dec!(1.50));

    assert_eq!(
        *alg.portfolio.cash.read(),
        dec!(100_300),
        "short premium should immediately credit cash"
    );

    let holding = alg.portfolio.get_holding(&option);
    assert_eq!(holding.quantity, dec!(-2));
    assert_eq!(holding.average_price, dec!(1.50));
    assert_eq!(holding.contract_multiplier, dec!(100));
    assert_eq!(holding.market_value(), dec!(-300));
    assert_eq!(alg.portfolio.total_portfolio_value(), dec!(100_000));
}

#[test]
fn buy_to_close_debits_cash_and_flattens_short_option_holding() {
    let mut alg = QcAlgorithm::new("test", dec!(100_000));
    let option = spy_put();

    alg.sell_to_open(option.clone(), dec!(2), dec!(1.50));
    alg.buy_to_close(option.clone(), dec!(2), dec!(0.50));

    assert_eq!(
        *alg.portfolio.cash.read(),
        dec!(100_200),
        "closing cost should reduce previously credited cash"
    );
    let holding = alg.portfolio.get_holding(&option);
    assert_eq!(holding.quantity, dec!(0));
    assert_eq!(holding.realized_pnl, dec!(200));
    assert_eq!(alg.portfolio.total_portfolio_value(), dec!(100_200));
}

#[test]
fn buy_to_open_debits_cash_and_sell_to_close_restores_it() {
    let mut alg = QcAlgorithm::new("test", dec!(100_000));
    let option = spy_put();

    alg.buy_to_open(option.clone(), dec!(1), dec!(2.25));
    assert_eq!(
        *alg.portfolio.cash.read(),
        dec!(99_775),
        "long option premium should be paid from cash"
    );

    alg.sell_to_close(option.clone(), dec!(1), dec!(1.10));
    assert_eq!(
        *alg.portfolio.cash.read(),
        dec!(99_885),
        "selling the long option should credit sale proceeds back to cash"
    );
    let holding = alg.portfolio.get_holding(&option);
    assert_eq!(holding.quantity, dec!(0));
    assert_eq!(holding.realized_pnl, dec!(-115));
    assert_eq!(alg.portfolio.total_portfolio_value(), dec!(99_885));
}

#[test]
fn option_holdings_are_marked_to_market_with_contract_multiplier() {
    let mut alg = QcAlgorithm::new("test", dec!(100_000));
    let option = spy_put();

    alg.sell_to_open(option.clone(), dec!(2), dec!(1.50));
    alg.portfolio.update_prices(&option, dec!(2.00));

    let holding = alg.portfolio.get_holding(&option);
    assert_eq!(holding.market_value(), dec!(-400));
    assert_eq!(holding.unrealized_pnl, dec!(-100));
    assert_eq!(alg.portfolio.total_portfolio_value(), dec!(99_900));
}
