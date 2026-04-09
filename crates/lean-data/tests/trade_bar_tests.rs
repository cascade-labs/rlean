use lean_core::{Market, NanosecondTimestamp, Resolution, Symbol, TimeSpan};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn make_bar(open: Decimal, high: Decimal, low: Decimal, close: Decimal, volume: Decimal) -> TradeBar {
    let time = NanosecondTimestamp::from_secs(1_700_000_000);
    TradeBar::new(spy(), time, TimeSpan::ONE_DAY, open, high, low, close, volume)
}

#[test]
fn bar_close_is_price() {
    let bar = make_bar(dec!(100), dec!(110), dec!(95), dec!(105), dec!(1_000_000));
    use lean_data::base_data::BaseData;
    assert_eq!(bar.price(), dec!(105));
}

#[test]
fn end_time_is_time_plus_period() {
    let time = NanosecondTimestamp::from_secs(1_700_000_000);
    let bar = TradeBar::new(spy(), time, TimeSpan::ONE_DAY, dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));
    assert_eq!(bar.end_time.0, bar.time.0 + TimeSpan::ONE_DAY.nanos);
}

#[test]
fn true_range_is_high_minus_low() {
    let bar = make_bar(dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));
    assert_eq!(bar.true_range(), dec!(20));
}

#[test]
fn spread_pct_of_close() {
    let bar = make_bar(dec!(100), dec!(110), dec!(90), dec!(100), dec!(1000));
    // (110 - 90) / 100 = 0.20
    let expected = dec!(0.20);
    assert!((bar.spread_pct() - expected).abs() < dec!(0.0001));
}

#[test]
fn is_valid_rejects_bad_bars() {
    // High < Low
    let bar = make_bar(dec!(100), dec!(90), dec!(110), dec!(100), dec!(1000));
    assert!(!bar.is_valid());

    // Negative low
    let bar2 = make_bar(dec!(100), dec!(110), dec!(-5), dec!(100), dec!(1000));
    assert!(!bar2.is_valid());

    // Normal bar
    let bar3 = make_bar(dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));
    assert!(bar3.is_valid());
}

#[test]
fn update_extends_high_and_low() {
    let mut bar = make_bar(dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));
    bar.update(dec!(120), dec!(500));
    assert_eq!(bar.high, dec!(120));
    assert_eq!(bar.close, dec!(120));
    assert_eq!(bar.volume, dec!(1500));

    bar.update(dec!(80), dec!(200));
    assert_eq!(bar.low, dec!(80));
}

#[test]
fn merge_combines_two_bars() {
    let t1 = NanosecondTimestamp::from_secs(1_700_000_000);
    let mut bar1 = TradeBar::new(spy(), t1, TimeSpan::ONE_DAY, dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));

    let t2 = t1 + TimeSpan::ONE_DAY;
    let bar2 = TradeBar::new(spy(), t2, TimeSpan::ONE_DAY, dec!(105), dec!(120), dec!(85), dec!(115), dec!(2000));

    bar1.merge(&bar2);

    assert_eq!(bar1.open, dec!(100));  // keeps first open
    assert_eq!(bar1.high, dec!(120));  // takes highest high
    assert_eq!(bar1.low, dec!(85));    // takes lowest low
    assert_eq!(bar1.close, dec!(115)); // takes last close
    assert_eq!(bar1.volume, dec!(3000));
    assert_eq!(bar1.end_time, bar2.end_time);
}

#[test]
fn from_lean_csv_line_parses_correctly() {
    // LEAN daily format: ms_since_midnight,open*10000,high*10000,low*10000,close*10000,volume
    let line = "34200000,4100000,4150000,4050000,4120000,5000000";
    let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
    let bar = TradeBar::from_lean_csv_line(line, spy(), date, Resolution::Daily);
    let bar = bar.expect("parse failed");

    assert_eq!(bar.open, dec!(410));
    assert_eq!(bar.high, dec!(415));
    assert_eq!(bar.low, dec!(405));
    assert_eq!(bar.close, dec!(412));
    assert_eq!(bar.volume, dec!(5000000));
}

#[test]
fn from_lean_csv_line_returns_none_for_bad_input() {
    let bar = TradeBar::from_lean_csv_line("bad,data", spy(),
        chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(), Resolution::Daily);
    assert!(bar.is_none());
}
