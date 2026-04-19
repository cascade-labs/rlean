use lean_core::{Market, NanosecondTimestamp, Symbol};
use lean_data::base_data::BaseData;
use lean_data::Tick;
use rust_decimal_macros::dec;

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}
fn now() -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(1_700_000_000)
}

#[test]
fn trade_tick_type_and_price() {
    let t = Tick::trade(spy(), now(), dec!(450), dec!(100));
    assert!(t.is_trade());
    assert!(!t.is_quote());
    assert_eq!(t.price(), dec!(450));
    assert_eq!(t.quantity, dec!(100));
}

#[test]
fn quote_tick_mid_price() {
    let t = Tick::quote(
        spy(),
        now(),
        dec!(449.98),
        dec!(450.02),
        dec!(500),
        dec!(300),
    );
    assert!(t.is_quote());
    // Mid = (449.98 + 450.02) / 2 = 450
    assert_eq!(t.price(), dec!(450));
    assert_eq!(t.bid_price, dec!(449.98));
    assert_eq!(t.ask_price, dec!(450.02));
}

#[test]
fn spread_calculation() {
    let t = Tick::quote(spy(), now(), dec!(100), dec!(100.10), dec!(100), dec!(100));
    assert_eq!(t.spread(), dec!(0.10));
}

#[test]
fn trade_tick_has_zero_spread() {
    let t = Tick::trade(spy(), now(), dec!(450), dec!(100));
    assert_eq!(t.spread(), dec!(0));
}

#[test]
fn from_lean_trade_csv_parses_correctly() {
    // format: ms,price*10000,quantity
    let line = "34200000,4500000,100";
    let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
    let tick = Tick::from_lean_trade_csv(line, spy(), date);
    let tick = tick.expect("parse failed");

    assert_eq!(tick.value, dec!(450));
    assert_eq!(tick.quantity, dec!(100));
}

#[test]
fn from_lean_quote_csv_parses_correctly() {
    // format: ms,bid*10000,ask*10000,bid_size,ask_size
    let line = "34200000,4499800,4500200,500,300";
    let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
    let tick = Tick::from_lean_quote_csv(line, spy(), date);
    let tick = tick.expect("parse failed");

    assert_eq!(tick.bid_price, dec!(449.98));
    assert_eq!(tick.ask_price, dec!(450.02));
    assert_eq!(tick.bid_size, dec!(500));
    assert_eq!(tick.ask_size, dec!(300));
}

#[test]
fn open_interest_tick() {
    let t = Tick::open_interest(spy(), now(), dec!(1_234_567));
    assert_eq!(t.value, dec!(1_234_567));
}
