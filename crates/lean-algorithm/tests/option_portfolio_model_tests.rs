use chrono::NaiveDate;
use lean_algorithm::portfolio::SecurityPortfolioManager;
use lean_core::{Market, OptionRight, OptionStyle, Symbol};
use rust_decimal_macros::dec;

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn spy_call() -> Symbol {
    Symbol::create_option(
        spy(),
        &Market::usa(),
        NaiveDate::from_ymd_opt(2016, 2, 19).unwrap(),
        dec!(192),
        OptionRight::Call,
        OptionStyle::American,
    )
}

fn spy_put() -> Symbol {
    Symbol::create_option(
        spy(),
        &Market::usa(),
        NaiveDate::from_ymd_opt(2016, 2, 19).unwrap(),
        dec!(192),
        OptionRight::Put,
        OptionStyle::American,
    )
}

// Port of Lean SecurityPortfolioManagerTests.InternalCallAssignmentAddsUnderlyingPositionAddsCash
#[test]
fn lean_internal_call_assignment_adds_underlying_position_adds_cash() {
    let pm = SecurityPortfolioManager::new(dec!(0));
    let underlying = spy();
    let option = spy_call();

    pm.set_holdings(&option, dec!(1), dec!(-1), dec!(100));
    pm.settle_fill_without_cash(&option, dec!(0), dec!(1), dec!(100));
    pm.apply_exercise_with_market_price(&underlying, dec!(192), dec!(-100), dec!(200));

    let underlying_holding = pm.get_holding(&underlying);
    let option_holding = pm.get_holding(&option);

    assert_eq!(*pm.cash.read(), dec!(19_200));
    assert_eq!(underlying_holding.quantity, dec!(-100));
    assert_eq!(underlying_holding.average_price, dec!(192));
    assert_eq!(option_holding.quantity, dec!(0));
}

// Port of Lean SecurityPortfolioManagerTests.InternalPutAssignmentAddsUnderlyingPositionReducesCash
#[test]
fn lean_internal_put_assignment_adds_underlying_position_reduces_cash() {
    let pm = SecurityPortfolioManager::new(dec!(19_200));
    let underlying = spy();
    let option = spy_put();

    pm.set_holdings(&option, dec!(1), dec!(-1), dec!(100));
    pm.settle_fill_without_cash(&option, dec!(0), dec!(1), dec!(100));
    pm.apply_exercise_with_market_price(&underlying, dec!(192), dec!(100), dec!(192));

    let underlying_holding = pm.get_holding(&underlying);
    let option_holding = pm.get_holding(&option);

    assert_eq!(*pm.cash.read(), dec!(0));
    assert_eq!(underlying_holding.quantity, dec!(100));
    assert_eq!(underlying_holding.average_price, dec!(192));
    assert_eq!(option_holding.quantity, dec!(0));
}

#[test]
fn physical_assignment_preserves_total_portfolio_value_at_market_price() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    let underlying = spy();
    let option = Symbol::create_option(
        underlying.clone(),
        &Market::usa(),
        NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
        dec!(450),
        OptionRight::Put,
        OptionStyle::American,
    );

    // Short 1 put for $1.50; mark it at $2.00 before assignment.
    *pm.cash.write() += dec!(150);
    pm.set_holdings(&option, dec!(1.50), dec!(-1), dec!(100));
    pm.update_prices(&option, dec!(2.00));

    let before = pm.total_portfolio_value();
    assert_eq!(before, dec!(99_950));

    // On expiry, the option disappears and the stock leg settles at strike,
    // but the resulting stock must still be marked at the current market price.
    pm.settle_fill_without_cash(&option, dec!(0), dec!(1), dec!(100));
    pm.apply_exercise_with_market_price(&underlying, dec!(450), dec!(100), dec!(448));

    let after = pm.total_portfolio_value();
    let stock = pm.get_holding(&underlying);
    let settled_option = pm.get_holding(&option);

    assert_eq!(after, before);
    assert_eq!(*pm.cash.read(), dec!(55_150));
    assert_eq!(stock.quantity, dec!(100));
    assert_eq!(stock.average_price, dec!(450));
    assert_eq!(stock.last_price, dec!(448));
    assert_eq!(settled_option.quantity, dec!(0));
}
