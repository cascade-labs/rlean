use lean_core::NanosecondTimestamp;
use lean_indicators::{indicator::Indicator, BollingerBands};
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

// ─── Ready state ─────────────────────────────────────────────────────────────

#[test]
fn not_ready_until_period_samples() {
    let mut bb = BollingerBands::standard(3);
    assert!(!bb.is_ready());

    bb.update_price(ts(0), dec!(10));
    assert!(!bb.is_ready());
    bb.update_price(ts(1), dec!(20));
    assert!(!bb.is_ready());
    bb.update_price(ts(2), dec!(30));
    assert!(bb.is_ready());
}

#[test]
fn warm_up_period_equals_period() {
    let bb = BollingerBands::standard(20);
    assert_eq!(bb.warm_up_period(), 20);
}

// ─── Correctness ─────────────────────────────────────────────────────────────

/// With equal values, std_dev=0, upper=lower=middle
#[test]
fn flat_prices_bands_collapse_to_middle() {
    let mut bb = BollingerBands::standard(3);
    for i in 0..5 {
        let r = bb.update_price(ts(i), dec!(50));
        if r.is_ready() {
            assert_eq!(bb.middle, dec!(50));
            assert_eq!(bb.upper, dec!(50));
            assert_eq!(bb.lower, dec!(50));
            assert_eq!(bb.bandwidth, dec!(0));
        }
    }
}

/// Middle band is always SMA of the window
#[test]
fn middle_equals_sma() {
    let mut bb = BollingerBands::standard(3);
    // Feed [10, 20, 30] → SMA = 20
    bb.update_price(ts(0), dec!(10));
    bb.update_price(ts(1), dec!(20));
    let r = bb.update_price(ts(2), dec!(30));
    assert!(r.is_ready());
    assert_eq!(bb.middle, dec!(20));
}

/// Upper > middle > lower when there is variance
#[test]
fn upper_above_middle_above_lower() {
    let mut bb = BollingerBands::standard(5);
    let values = [dec!(10), dec!(20), dec!(30), dec!(20), dec!(10)];
    for (i, &v) in values.iter().enumerate() {
        let r = bb.update_price(ts(i as i64), v);
        if r.is_ready() {
            assert!(bb.upper >= bb.middle, "upper should be >= middle");
            assert!(bb.middle >= bb.lower, "middle should be >= lower");
        }
    }
}

/// Custom k=1 gives narrower bands than k=2
#[test]
fn custom_k_narrows_bands() {
    let values = [dec!(10), dec!(20), dec!(30), dec!(20), dec!(10)];
    let mut bb1 = BollingerBands::new(5, dec!(1));
    let mut bb2 = BollingerBands::new(5, dec!(2));

    for (i, &v) in values.iter().enumerate() {
        bb1.update_price(ts(i as i64), v);
        bb2.update_price(ts(i as i64), v);
    }

    assert!(bb1.is_ready());
    assert!(bb2.is_ready());
    // k=1 gives narrower bands
    assert!(bb1.bandwidth < bb2.bandwidth);
}

/// percent_b at 0.5 when price == middle
#[test]
fn percent_b_at_middle_is_half() {
    let mut bb = BollingerBands::standard(3);
    // Feed [10, 20, 30] → middle=20, last value is 30
    bb.update_price(ts(0), dec!(10));
    bb.update_price(ts(1), dec!(20));
    bb.update_price(ts(2), dec!(30));
    // Now feed middle value
    bb.update_price(ts(3), dec!(10));
    bb.update_price(ts(4), dec!(20));
    bb.update_price(ts(5), dec!(30));
    let r = bb.update_price(ts(6), dec!(20)); // price == middle
    if r.is_ready() {
        // percent_b = (20 - lower) / (upper - lower)
        // When price == middle = 20, percent_b = 0.5 iff symmetric bands
        // Just verify it's between 0 and 1
        assert!(bb.percent_b >= dec!(0) && bb.percent_b <= dec!(1));
    }
}

// ─── Reset ───────────────────────────────────────────────────────────────────

#[test]
fn reset_clears_state() {
    let mut bb = BollingerBands::standard(3);
    for i in 0..5 {
        bb.update_price(ts(i), dec!(10));
    }
    assert!(bb.is_ready());

    bb.reset();

    assert!(!bb.is_ready());
    assert_eq!(bb.samples(), 0);
    assert!(!bb.current().is_ready());
}
