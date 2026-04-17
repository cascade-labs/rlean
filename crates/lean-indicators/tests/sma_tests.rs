use lean_core::NanosecondTimestamp;
use lean_indicators::{indicator::Indicator, Sma};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

fn feed(sma: &mut Sma, values: &[Decimal]) -> Vec<Option<Decimal>> {
    values
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let r = sma.update_price(ts(i as i64), v);
            if r.is_ready() {
                Some(r.value)
            } else {
                None
            }
        })
        .collect()
}

// ─── Ready state ─────────────────────────────────────────────────────────────

#[test]
fn not_ready_until_period_samples() {
    let mut sma = Sma::new(3);
    assert!(!sma.is_ready());

    sma.update_price(ts(0), dec!(1));
    assert!(!sma.is_ready());

    sma.update_price(ts(1), dec!(2));
    assert!(!sma.is_ready());

    sma.update_price(ts(2), dec!(3));
    assert!(sma.is_ready());
}

#[test]
fn warm_up_period_equals_period() {
    let sma = Sma::new(14);
    assert_eq!(sma.warm_up_period(), 14);
}

#[test]
fn sample_count_increments_correctly() {
    let mut sma = Sma::new(3);
    for i in 0..5 {
        sma.update_price(ts(i), dec!(1));
    }
    assert_eq!(sma.samples(), 5);
}

// ─── Correctness ─────────────────────────────────────────────────────────────

/// Mirrors LEAN's SmaComputesCorrectly test:
/// period=4, values=[1,10,100,1000,10000,1234,56789]
#[test]
fn sma_computes_correctly_period_4() {
    let mut sma = Sma::new(4);
    let values = [
        dec!(1),
        dec!(10),
        dec!(100),
        dec!(1000),
        dec!(10000),
        dec!(1234),
        dec!(56789),
    ];

    let results = feed(&mut sma, &values);

    // First 3 not ready
    assert!(results[0].is_none());
    assert!(results[1].is_none());
    assert!(results[2].is_none());

    // i=3: (1+10+100+1000)/4 = 277.75
    assert_eq!(results[3].unwrap(), dec!(277.75));

    // i=4: (10+100+1000+10000)/4 = 2777.5
    assert_eq!(results[4].unwrap(), dec!(2777.5));

    // i=5: (100+1000+10000+1234)/4 = 3083.5
    assert_eq!(results[5].unwrap(), dec!(3083.5));

    // i=6: (1000+10000+1234+56789)/4 = 17255.75
    assert_eq!(results[6].unwrap(), dec!(17255.75));
}

#[test]
fn sma_period_1_returns_each_value() {
    let mut sma = Sma::new(1);
    for val in [dec!(5), dec!(10), dec!(3)] {
        let r = sma.update_price(ts(0), val);
        assert!(r.is_ready());
        assert_eq!(r.value, val);
    }
}

#[test]
fn sma_of_equal_values_equals_that_value() {
    let mut sma = Sma::new(5);
    for i in 0..10 {
        let r = sma.update_price(ts(i), dec!(42));
        if sma.is_ready() {
            assert_eq!(r.value, dec!(42));
        }
    }
}

#[test]
fn sma_running_sum_is_efficient() {
    // Verify the rolling-sum approach gives same answer as naive average
    let mut sma = Sma::new(3);
    let values = [dec!(10), dec!(20), dec!(30), dec!(40), dec!(50)];

    let results = feed(&mut sma, &values);

    // (10+20+30)/3 = 20
    assert_eq!(results[2].unwrap(), dec!(20));
    // (20+30+40)/3 = 30
    assert_eq!(results[3].unwrap(), dec!(30));
    // (30+40+50)/3 = 40
    assert_eq!(results[4].unwrap(), dec!(40));
}

// ─── Reset ───────────────────────────────────────────────────────────────────

#[test]
fn reset_clears_state() {
    let mut sma = Sma::new(3);
    feed(&mut sma, &[dec!(1), dec!(2), dec!(3), dec!(4)]);
    assert!(sma.is_ready());
    assert_eq!(sma.samples(), 4);

    sma.reset();

    assert!(!sma.is_ready());
    assert_eq!(sma.samples(), 0);
    assert!(!sma.current().is_ready());
}

#[test]
fn after_reset_computes_correctly_again() {
    let mut sma = Sma::new(2);
    feed(&mut sma, &[dec!(10), dec!(20)]);
    assert_eq!(sma.current().value, dec!(15));

    sma.reset();

    let r1 = sma.update_price(ts(0), dec!(100));
    assert!(!r1.is_ready());

    let r2 = sma.update_price(ts(1), dec!(200));
    assert!(r2.is_ready());
    assert_eq!(r2.value, dec!(150));
}

// ─── Edge cases ──────────────────────────────────────────────────────────────

#[test]
fn sma_handles_zero_values() {
    let mut sma = Sma::new(3);
    let results = feed(&mut sma, &[dec!(0), dec!(0), dec!(0), dec!(0)]);
    assert_eq!(results[2].unwrap(), dec!(0));
    assert_eq!(results[3].unwrap(), dec!(0));
}

#[test]
fn sma_handles_negative_values() {
    let mut sma = Sma::new(3);
    let results = feed(&mut sma, &[dec!(-3), dec!(-6), dec!(-9)]);
    assert_eq!(results[2].unwrap(), dec!(-6));
}

#[test]
fn sma_handles_fractional_values() {
    let mut sma = Sma::new(4);
    let results = feed(&mut sma, &[dec!(1.5), dec!(2.5), dec!(3.5), dec!(4.5)]);
    // (1.5+2.5+3.5+4.5)/4 = 3.0
    assert_eq!(results[3].unwrap(), dec!(3));
}
