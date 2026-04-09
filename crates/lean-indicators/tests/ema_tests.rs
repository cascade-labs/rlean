use lean_indicators::{indicator::Indicator, Ema};
use lean_core::NanosecondTimestamp;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp { NanosecondTimestamp::from_secs(i * 86400) }

fn feed(ema: &mut Ema, values: &[Decimal]) -> Vec<Option<Decimal>> {
    values.iter().enumerate().map(|(i, &v)| {
        let r = ema.update_price(ts(i as i64), v);
        if r.is_ready() { Some(r.value) } else { None }
    }).collect()
}

// ─── Ready state ─────────────────────────────────────────────────────────────

#[test]
fn not_ready_until_period_samples() {
    let mut ema = Ema::new(4);
    assert!(!ema.is_ready());

    for i in 0..3 {
        ema.update_price(ts(i), dec!(10));
        assert!(!ema.is_ready());
    }
    ema.update_price(ts(3), dec!(10));
    assert!(ema.is_ready());
}

#[test]
fn warm_up_period_equals_period() {
    let ema = Ema::new(14);
    assert_eq!(ema.warm_up_period(), 14);
}

#[test]
fn multiplier_is_correct() {
    // multiplier = 2 / (period + 1)
    let ema = Ema::new(4);
    // 2/5 = 0.4
    assert_eq!(ema.multiplier(), dec!(2) / dec!(5));
}

// ─── Correctness ─────────────────────────────────────────────────────────────

/// period=1: every value is immediately returned (EMA seeds on first value, period=1 means ready after 1)
#[test]
fn ema_period_1_returns_each_value() {
    let mut ema = Ema::new(1);
    for val in [dec!(5), dec!(10), dec!(3)] {
        let r = ema.update_price(ts(0), val);
        assert!(r.is_ready());
        // period=1, mult = 2/2 = 1: EMA = (val - prev) * 1 + prev = val
        assert_eq!(r.value, val);
    }
}

/// period=2: multiplier = 2/3
/// s1: cv=10, not ready
/// s2: cv=(20-10)*(2/3)+10 = 6.666...+10 = 16.666..., ready
/// s3: cv=(30-16.666...)*(2/3)+16.666... ≈ 8.888...+16.666... ≈ 25.555...
#[test]
fn ema_period_2_correctness() {
    let mut ema = Ema::new(2);
    let mult = dec!(2) / dec!(3);

    let r1 = ema.update_price(ts(0), dec!(10));
    assert!(!r1.is_ready());

    let r2 = ema.update_price(ts(1), dec!(20));
    assert!(r2.is_ready());
    let expected2 = (dec!(20) - dec!(10)) * mult + dec!(10);
    assert_eq!(r2.value, expected2);

    let r3 = ema.update_price(ts(2), dec!(30));
    let expected3 = (dec!(30) - expected2) * mult + expected2;
    assert_eq!(r3.value, expected3);
}

/// All equal values → EMA = that value once seeded
#[test]
fn ema_of_equal_values() {
    let mut ema = Ema::new(5);
    for i in 0..10 {
        let r = ema.update_price(ts(i), dec!(42));
        if ema.is_ready() {
            assert_eq!(r.value, dec!(42));
        }
    }
}

// ─── Reset ───────────────────────────────────────────────────────────────────

#[test]
fn reset_clears_state() {
    let mut ema = Ema::new(3);
    feed(&mut ema, &[dec!(1), dec!(2), dec!(3), dec!(4)]);
    assert!(ema.is_ready());

    ema.reset();

    assert!(!ema.is_ready());
    assert_eq!(ema.samples(), 0);
    assert!(!ema.current().is_ready());
}

#[test]
fn after_reset_computes_correctly_again() {
    let mut ema = Ema::new(2);
    let mult = dec!(2) / dec!(3);

    // First run
    ema.update_price(ts(0), dec!(10));
    let r = ema.update_price(ts(1), dec!(20));
    let first_ema = r.value;
    assert!(r.is_ready());

    ema.reset();

    // Second run — same sequence should give same result
    ema.update_price(ts(0), dec!(10));
    let r2 = ema.update_price(ts(1), dec!(20));
    assert!(r2.is_ready());
    assert_eq!(r2.value, first_ema);
    let _ = mult; // suppress unused warning
}
