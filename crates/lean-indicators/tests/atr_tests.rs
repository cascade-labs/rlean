use lean_core::{Market, NanosecondTimestamp, Symbol};
use lean_data::TradeBar;
use lean_indicators::{indicator::Indicator, Atr};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

fn make_bar(i: i64, high: Decimal, low: Decimal, close: Decimal) -> TradeBar {
    TradeBar {
        symbol: Symbol::create_equity("SPY", &Market::usa()),
        time: ts(i),
        end_time: ts(i + 1),
        open: (high + low) / dec!(2),
        high,
        low,
        close,
        volume: dec!(1000),
        period: lean_core::TimeSpan::ONE_DAY,
    }
}

// ─── Ready state ─────────────────────────────────────────────────────────────

#[test]
fn not_ready_until_period_plus_one_bars() {
    let mut atr = Atr::new(3);
    assert!(!atr.is_ready());

    // period+1 = 4 bars required
    for i in 0..3 {
        atr.update_bar(&make_bar(i, dec!(105), dec!(95), dec!(100)));
        assert!(!atr.is_ready());
    }
    atr.update_bar(&make_bar(3, dec!(105), dec!(95), dec!(100)));
    assert!(atr.is_ready());
}

#[test]
fn warm_up_period_equals_period_plus_one() {
    let atr = Atr::new(14);
    assert_eq!(atr.warm_up_period(), 15);
}

// ─── Correctness ─────────────────────────────────────────────────────────────

/// When bars have no gap (close == open of next), TR = H - L
/// ATR after seeding = avg of first period TRs, then Wilder smoothing
#[test]
fn atr_with_constant_range() {
    let mut atr = Atr::new(3);
    // Range = 10 on every bar, no gaps
    // First bar has no prev_close so TR = H - L = 10
    for i in 0..5 {
        let close = dec!(100);
        atr.update_bar(&make_bar(i, dec!(105), dec!(95), close));
    }

    assert!(atr.is_ready());
    // Initial ATR seeds = avg of first 3 TRs = 10 each → ATR = 10
    // Wilder: (10*(3-1) + 10) / 3 = 10 → remains 10
    assert_eq!(atr.current().value, dec!(10));
}

/// ATR must be > 0 for non-degenerate bars
#[test]
fn atr_positive_for_real_bars() {
    let mut atr = Atr::new(5);
    let bars = [
        (dec!(105), dec!(95), dec!(102)),
        (dec!(108), dec!(98), dec!(106)),
        (dec!(110), dec!(100), dec!(108)),
        (dec!(107), dec!(97), dec!(103)),
        (dec!(112), dec!(102), dec!(110)),
        (dec!(109), dec!(99), dec!(105)),
    ];
    for (i, (h, l, c)) in bars.iter().enumerate() {
        atr.update_bar(&make_bar(i as i64, *h, *l, *c));
    }
    assert!(atr.is_ready());
    assert!(atr.current().value > dec!(0));
}

// ─── Reset ───────────────────────────────────────────────────────────────────

#[test]
fn reset_clears_state() {
    let mut atr = Atr::new(3);
    for i in 0..5 {
        atr.update_bar(&make_bar(i, dec!(105), dec!(95), dec!(100)));
    }
    assert!(atr.is_ready());

    atr.reset();

    assert!(!atr.is_ready());
    assert_eq!(atr.samples(), 0);
    assert!(!atr.current().is_ready());
}

#[test]
fn after_reset_computes_correctly_again() {
    let mut atr = Atr::new(3);
    for i in 0..5 {
        atr.update_bar(&make_bar(i, dec!(105), dec!(95), dec!(100)));
    }
    let first_val = atr.current().value;
    assert!(atr.is_ready());

    atr.reset();

    for i in 0..5 {
        atr.update_bar(&make_bar(i, dec!(105), dec!(95), dec!(100)));
    }
    assert_eq!(atr.current().value, first_val);
}
