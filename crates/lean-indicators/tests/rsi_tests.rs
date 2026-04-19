use lean_core::NanosecondTimestamp;
use lean_indicators::{indicator::Indicator, Rsi};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

// ─── Ready state ─────────────────────────────────────────────────────────────

#[test]
fn not_ready_until_period_plus_one() {
    // RSI needs period+1 samples (period diffs) before first output
    let mut rsi = Rsi::new(3);
    assert!(!rsi.is_ready());

    for i in 0..4 {
        rsi.update_price(ts(i), dec!(10));
    }
    // samples=4 = period+1, but warm_up_period = period+1 = 4
    // is_ready checks samples > period (>3) → 4 > 3 → true
    assert!(rsi.is_ready());
}

#[test]
fn warm_up_period_equals_period_plus_one() {
    let rsi = Rsi::new(14);
    assert_eq!(rsi.warm_up_period(), 15);
}

// ─── Correctness ─────────────────────────────────────────────────────────────

/// All upward moves → RSI = 100
#[test]
fn all_gains_gives_rsi_100() {
    let mut rsi = Rsi::new(3);
    // Feed strictly increasing values so all changes are gains
    let values = [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)];
    let mut last_result = None;
    for (i, &v) in values.iter().enumerate() {
        let r = rsi.update_price(ts(i as i64), v);
        if r.is_ready() {
            last_result = Some(r.value);
        }
    }
    assert_eq!(last_result.unwrap(), dec!(100));
}

/// All downward moves → RSI = 0
#[test]
fn all_losses_gives_rsi_0() {
    let mut rsi = Rsi::new(3);
    let values = [dec!(50), dec!(40), dec!(30), dec!(20), dec!(10)];
    let mut last_result = None;
    for (i, &v) in values.iter().enumerate() {
        let r = rsi.update_price(ts(i as i64), v);
        if r.is_ready() {
            last_result = Some(r.value);
        }
    }
    assert_eq!(last_result.unwrap(), dec!(0));
}

/// No change → RSI indeterminate but avg_loss = 0, so RSI = 100
#[test]
fn flat_prices_gives_rsi_100() {
    let mut rsi = Rsi::new(3);
    let values = [dec!(50), dec!(50), dec!(50), dec!(50), dec!(50)];
    let mut last_result = None;
    for (i, &v) in values.iter().enumerate() {
        let r = rsi.update_price(ts(i as i64), v);
        if r.is_ready() {
            last_result = Some(r.value);
        }
    }
    assert_eq!(last_result.unwrap(), dec!(100));
}

/// RSI bounded between 0 and 100
#[test]
fn rsi_bounded_0_to_100() {
    let mut rsi = Rsi::new(5);
    let values = [
        dec!(10),
        dec!(20),
        dec!(15),
        dec!(25),
        dec!(18),
        dec!(30),
        dec!(22),
        dec!(35),
        dec!(28),
        dec!(40),
    ];
    for (i, &v) in values.iter().enumerate() {
        let r = rsi.update_price(ts(i as i64), v);
        if r.is_ready() {
            assert!(r.value >= dec!(0), "RSI below 0: {}", r.value);
            assert!(r.value <= dec!(100), "RSI above 100: {}", r.value);
        }
    }
}

#[test]
fn overbought_oversold_thresholds() {
    let mut rsi = Rsi::new(3);
    // All gains — should be overbought
    for i in 0..5 {
        rsi.update_price(ts(i), Decimal::from(i * 10 + 10));
    }
    assert!(rsi.is_overbought());
    assert!(!rsi.is_oversold());
}

// ─── Reset ───────────────────────────────────────────────────────────────────

#[test]
fn reset_clears_state() {
    let mut rsi = Rsi::new(3);
    for i in 0..5 {
        rsi.update_price(ts(i), Decimal::from(i * 10));
    }
    assert!(rsi.is_ready());

    rsi.reset();

    assert!(!rsi.is_ready());
    assert_eq!(rsi.samples(), 0);
}
