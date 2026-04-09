use lean_core::{Market, OptionRight, OptionStyle, SecurityType, Symbol};
use rust_decimal_macros::dec;
use chrono::NaiveDate;

#[test]
fn create_equity_symbol_has_correct_type() {
    let market = Market::usa();
    let sym = Symbol::create_equity("SPY", &market);
    assert_eq!(sym.security_type(), SecurityType::Equity);
    assert_eq!(sym.value, "SPY");
    assert_eq!(sym.market().as_str(), "usa");
}

#[test]
fn create_equity_ticker_uppercased() {
    let market = Market::usa();
    let sym = Symbol::create_equity("spy", &market);
    assert_eq!(sym.value, "SPY");
}

#[test]
fn create_forex_symbol_has_correct_type() {
    let sym = Symbol::create_forex("EURUSD");
    assert_eq!(sym.security_type(), SecurityType::Forex);
    assert_eq!(sym.market().as_str(), "forex");
}

#[test]
fn create_crypto_symbol_has_correct_type() {
    let market = Market::binance();
    let sym = Symbol::create_crypto("BTCUSDT", &market);
    assert_eq!(sym.security_type(), SecurityType::Crypto);
    assert_eq!(sym.market().as_str(), "binance");
}

#[test]
fn create_future_symbol_has_expiry() {
    let market = Market::cme();
    let expiry = NaiveDate::from_ymd_opt(2024, 12, 20).unwrap();
    let sym = Symbol::create_future("ES", &market, expiry);
    assert_eq!(sym.security_type(), SecurityType::Future);
    assert_eq!(sym.id.expiry, Some(expiry));
}

#[test]
fn create_option_symbol_has_correct_fields() {
    let market = Market::usa();
    let underlying = Symbol::create_equity("AAPL", &market);
    let expiry = NaiveDate::from_ymd_opt(2024, 6, 21).unwrap();
    let strike = dec!(180);

    let opt = Symbol::create_option(
        underlying,
        &market,
        expiry,
        strike,
        OptionRight::Call,
        OptionStyle::American,
    );

    assert_eq!(opt.security_type(), SecurityType::Option);
    assert_eq!(opt.id.strike, Some(strike));
    assert_eq!(opt.id.option_right, Some(OptionRight::Call));
    assert_eq!(opt.id.option_style, Some(OptionStyle::American));
    assert_eq!(opt.id.expiry, Some(expiry));
    assert!(opt.has_underlying());
}

#[test]
fn two_identical_equity_symbols_are_equal() {
    let market = Market::usa();
    let a = Symbol::create_equity("AAPL", &market);
    let b = Symbol::create_equity("AAPL", &market);
    assert_eq!(a, b);
    assert_eq!(a.id.sid, b.id.sid);
}

#[test]
fn different_equity_symbols_are_not_equal() {
    let market = Market::usa();
    let a = Symbol::create_equity("AAPL", &market);
    let b = Symbol::create_equity("MSFT", &market);
    assert_ne!(a, b);
    assert_ne!(a.id.sid, b.id.sid);
}

#[test]
fn same_ticker_different_market_not_equal() {
    let a = Symbol::create_equity("BTC", &Market::new("usa"));
    let b = Symbol::create_crypto("BTC", &Market::binance());
    assert_ne!(a, b);
}

#[test]
fn symbol_can_be_used_as_hashmap_key() {
    use std::collections::HashMap;
    let market = Market::usa();
    let mut map = HashMap::new();

    let spy = Symbol::create_equity("SPY", &market);
    let aapl = Symbol::create_equity("AAPL", &market);

    map.insert(spy.clone(), 100.0f64);
    map.insert(aapl.clone(), 200.0f64);

    assert_eq!(map[&spy], 100.0);
    assert_eq!(map[&aapl], 200.0);
}
