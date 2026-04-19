use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::{Slice, Tick, TradeBar, TradeBarData};
use rust_decimal_macros::dec;

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}
fn aapl() -> Symbol {
    Symbol::create_equity("AAPL", &Market::usa())
}
fn t() -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(1_700_000_000)
}

fn make_bar(sym: Symbol, close: rust_decimal::Decimal) -> TradeBar {
    TradeBar::new(
        sym,
        t(),
        TimeSpan::ONE_DAY,
        TradeBarData::new(close, close, close, close, dec!(1000)),
    )
}

#[test]
fn empty_slice_has_no_data() {
    let slice = Slice::new(t());
    assert!(!slice.has_data);
}

#[test]
fn add_bar_sets_has_data() {
    let mut slice = Slice::new(t());
    slice.add_bar(make_bar(spy(), dec!(450)));
    assert!(slice.has_data);
}

#[test]
fn get_bar_returns_correct_bar() {
    let mut slice = Slice::new(t());
    let spy = spy();
    let aapl = aapl();

    slice.add_bar(make_bar(spy.clone(), dec!(450)));
    slice.add_bar(make_bar(aapl.clone(), dec!(190)));

    let spy_bar = slice.get_bar(&spy).expect("spy bar missing");
    assert_eq!(spy_bar.close, dec!(450));

    let aapl_bar = slice.get_bar(&aapl).expect("aapl bar missing");
    assert_eq!(aapl_bar.close, dec!(190));
}

#[test]
fn get_bar_returns_none_for_unsubscribed() {
    let mut slice = Slice::new(t());
    slice.add_bar(make_bar(spy(), dec!(450)));

    let msft = Symbol::create_equity("MSFT", &Market::usa());
    assert!(slice.get_bar(&msft).is_none());
}

#[test]
fn multiple_ticks_accumulate_per_symbol() {
    let mut slice = Slice::new(t());
    let spy = spy();

    slice.add_tick(Tick::trade(spy.clone(), t(), dec!(450), dec!(100)));
    slice.add_tick(Tick::trade(spy.clone(), t(), dec!(451), dec!(200)));

    let ticks = slice.get_ticks(&spy).expect("no ticks");
    assert_eq!(ticks.len(), 2);
    assert_eq!(ticks[0].value, dec!(450));
    assert_eq!(ticks[1].value, dec!(451));
}

#[test]
fn add_dividend_stored_correctly() {
    let mut slice = Slice::new(t());
    let spy = spy();
    let div = lean_data::Dividend::new(spy.clone(), t(), dec!(1.50), dec!(450));
    slice.add_dividend(div);

    assert!(slice.dividends.contains_key(&spy.id.sid));
}
