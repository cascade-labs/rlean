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
fn open_interest_tick() {
    let t = Tick::open_interest(spy(), now(), dec!(1_234_567));
    assert_eq!(t.value, dec!(1_234_567));
}
