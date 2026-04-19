use lean_core::NanosecondTimestamp;
use lean_indicators::{indicator::Indicator, Macd};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

// ─── Ready state ─────────────────────────────────────────────────────────────

#[test]
fn not_ready_until_slow_plus_signal_samples() {
    let mut macd = Macd::new(3, 5, 2);
    // warm_up_period declared as slow + signal = 5 + 2 = 7
    assert_eq!(macd.warm_up_period(), 7);
    assert!(!macd.is_ready());

    // Feed 5 values — slow EMA just became ready, signal has 1 sample
    for i in 0..5 {
        macd.update_price(ts(i), dec!(10));
    }
    assert!(!macd.is_ready());

    // 6th value: signal EMA reaches 2 samples → both ready
    macd.update_price(ts(5), dec!(10));
    assert!(macd.is_ready());
}

// ─── Correctness ─────────────────────────────────────────────────────────────

/// With equal values, MACD line = 0 (fast EMA = slow EMA), histogram = 0
#[test]
fn macd_flat_prices_gives_zero_line() {
    let mut macd = Macd::new(3, 5, 2);
    for i in 0..10 {
        let r = macd.update_price(ts(i), dec!(42));
        if r.is_ready() {
            assert_eq!(
                macd.macd_line,
                dec!(0),
                "MACD line should be 0 for flat prices"
            );
            assert_eq!(
                macd.histogram,
                dec!(0),
                "Histogram should be 0 for flat prices"
            );
        }
    }
}

/// MACD line is positive when fast EMA > slow EMA (prices rising)
#[test]
fn macd_rising_prices_positive_line() {
    let mut macd = Macd::new(3, 5, 2);
    for i in 0..15 {
        let r = macd.update_price(ts(i), Decimal::from(i * 10));
        if r.is_ready() {
            assert!(
                macd.macd_line > dec!(0),
                "MACD should be positive for rising prices"
            );
        }
    }
}

/// MACD line is negative when fast EMA < slow EMA (prices falling)
#[test]
fn macd_falling_prices_negative_line() {
    let mut macd = Macd::new(3, 5, 2);
    for i in 0..15 {
        let r = macd.update_price(ts(i), Decimal::from((14 - i) * 10));
        if r.is_ready() {
            assert!(
                macd.macd_line < dec!(0),
                "MACD should be negative for falling prices"
            );
        }
    }
}

// ─── Reset ───────────────────────────────────────────────────────────────────

#[test]
fn reset_clears_state() {
    let mut macd = Macd::new(3, 5, 2);
    for i in 0..10 {
        macd.update_price(ts(i), dec!(10));
    }
    assert!(macd.is_ready());

    macd.reset();

    assert!(!macd.is_ready());
    assert_eq!(macd.samples(), 0);
    assert_eq!(macd.macd_line, dec!(0));
    assert_eq!(macd.signal_line, dec!(0));
    assert_eq!(macd.histogram, dec!(0));
}

#[test]
fn histogram_equals_macd_minus_signal() {
    let mut macd = Macd::new(3, 5, 2);
    for i in 0..15 {
        let r = macd.update_price(ts(i), Decimal::from(i * 5 + 100));
        if r.is_ready() {
            let expected_hist = macd.macd_line - macd.signal_line;
            assert_eq!(macd.histogram, expected_hist);
        }
    }
}
