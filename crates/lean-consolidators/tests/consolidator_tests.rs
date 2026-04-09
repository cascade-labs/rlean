/// Comprehensive integration tests for lean-consolidators.
///
/// Covers TradeBarConsolidator (count + time-period), RenkoConsolidator (rising,
/// falling, reversal, multi-brick), VolumeConsolidator, HeikinAshiConsolidator,
/// and CalendarConsolidator (daily, weekly, monthly).

use lean_consolidators::{
    CalendarConsolidator, CalendarPeriod, HeikinAshiConsolidator, IConsolidator,
    RenkoConsolidator, TradeBarConsolidator, VolumeConsolidator,
};
use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::TradeBar;
use rust_decimal_macros::dec;

// ─────────────────────────────────────────────────────────────────────────────
// Shared helpers
// ─────────────────────────────────────────────────────────────────────────────

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn ts(nanos: i64) -> NanosecondTimestamp {
    NanosecondTimestamp(nanos)
}

/// Build a TradeBar with a 1-minute period.
fn bar_at(
    symbol: Symbol,
    time_nanos: i64,
    open: rust_decimal::Decimal,
    high: rust_decimal::Decimal,
    low: rust_decimal::Decimal,
    close: rust_decimal::Decimal,
    volume: rust_decimal::Decimal,
) -> TradeBar {
    let period = TimeSpan::ONE_MINUTE;
    TradeBar {
        symbol,
        time: ts(time_nanos),
        end_time: ts(time_nanos + period.nanos),
        open,
        high,
        low,
        close,
        volume,
        period,
    }
}

/// Build a SPY bar at minute offset `min` with a 1-minute period.
fn spy_bar(
    min: i64,
    open: rust_decimal::Decimal,
    high: rust_decimal::Decimal,
    low: rust_decimal::Decimal,
    close: rust_decimal::Decimal,
    volume: rust_decimal::Decimal,
) -> TradeBar {
    let nanos = min * 60_000_000_000_i64;
    bar_at(spy(), nanos, open, high, low, close, volume)
}

/// Build a SPY bar at a raw nanosecond timestamp with a 1-day period.
fn daily_bar_at(
    time_nanos: i64,
    open: rust_decimal::Decimal,
    high: rust_decimal::Decimal,
    low: rust_decimal::Decimal,
    close: rust_decimal::Decimal,
    volume: rust_decimal::Decimal,
) -> TradeBar {
    let period = TimeSpan::ONE_DAY;
    TradeBar {
        symbol: spy(),
        time: ts(time_nanos),
        end_time: ts(time_nanos + period.nanos),
        open,
        high,
        low,
        close,
        volume,
        period,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// TradeBarConsolidator — count mode
// ─────────────────────────────────────────────────────────────────────────────

mod trade_bar_consolidator_tests {
    use super::*;

    #[test]
    fn count_consolidator_no_emit_before_n_bars() {
        // n=3: first two updates should return None
        let mut c = TradeBarConsolidator::new_count(3);
        assert!(c.update(&spy_bar(0, dec!(1), dec!(2), dec!(0.5), dec!(1.0), dec!(100))).is_none());
        assert!(c.update(&spy_bar(1, dec!(2), dec!(3), dec!(1.5), dec!(2.0), dec!(200))).is_none());
    }

    #[test]
    fn count_consolidator_emits_on_nth_bar() {
        // n=3: third update should return Some
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(1), dec!(2), dec!(0.5), dec!(1.0), dec!(100)));
        c.update(&spy_bar(1, dec!(2), dec!(3), dec!(1.5), dec!(2.0), dec!(200)));
        let result = c.update(&spy_bar(2, dec!(3), dec!(4), dec!(2.5), dec!(3.0), dec!(300)));
        assert!(result.is_some(), "should emit after 3rd bar");
    }

    #[test]
    fn consolidated_open_is_first_bars_open() {
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(10), dec!(100), dec!(1), dec!(50), dec!(75)));
        c.update(&spy_bar(1, dec!(50), dec!(123), dec!(35), dec!(75), dec!(100)));
        let out = c
            .update(&spy_bar(2, dec!(75), dec!(100), dec!(50), dec!(83), dec!(125)))
            .expect("should emit");
        assert_eq!(out.open, dec!(10), "open must be first bar's open");
    }

    #[test]
    fn consolidated_high_is_max_of_all_highs() {
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(10), dec!(100), dec!(1), dec!(50), dec!(75)));
        c.update(&spy_bar(1, dec!(50), dec!(123), dec!(35), dec!(75), dec!(100)));
        let out = c
            .update(&spy_bar(2, dec!(75), dec!(100), dec!(50), dec!(83), dec!(125)))
            .expect("should emit");
        assert_eq!(out.high, dec!(123), "high must be max of all input highs");
    }

    #[test]
    fn consolidated_low_is_min_of_all_lows() {
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(10), dec!(100), dec!(1), dec!(50), dec!(75)));
        c.update(&spy_bar(1, dec!(50), dec!(123), dec!(35), dec!(75), dec!(100)));
        let out = c
            .update(&spy_bar(2, dec!(75), dec!(100), dec!(50), dec!(83), dec!(125)))
            .expect("should emit");
        assert_eq!(out.low, dec!(1), "low must be min of all input lows");
    }

    #[test]
    fn consolidated_close_is_last_bars_close() {
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(10), dec!(100), dec!(1), dec!(50), dec!(75)));
        c.update(&spy_bar(1, dec!(50), dec!(123), dec!(35), dec!(75), dec!(100)));
        let out = c
            .update(&spy_bar(2, dec!(75), dec!(100), dec!(50), dec!(83), dec!(125)))
            .expect("should emit");
        assert_eq!(out.close, dec!(83), "close must be last bar's close");
    }

    #[test]
    fn consolidated_volume_is_sum_of_all_volumes() {
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(10), dec!(100), dec!(1), dec!(50), dec!(75)));
        c.update(&spy_bar(1, dec!(50), dec!(123), dec!(35), dec!(75), dec!(100)));
        let out = c
            .update(&spy_bar(2, dec!(75), dec!(100), dec!(50), dec!(83), dec!(125)))
            .expect("should emit");
        assert_eq!(out.volume, dec!(300), "volume must be sum of all input volumes");
    }

    #[test]
    fn count_consolidator_resets_and_emits_again_after_n_more_bars() {
        let mut c = TradeBarConsolidator::new_count(3);
        for i in 0..3 {
            c.update(&spy_bar(i, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10)));
        }
        assert!(c.update(&spy_bar(3, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_none());
        assert!(c.update(&spy_bar(4, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_none());
        let out = c.update(&spy_bar(5, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10)));
        assert!(out.is_some(), "should emit again after 3 more bars");
    }

    #[test]
    fn count_one_emits_every_bar() {
        let mut c = TradeBarConsolidator::new_count(1);
        for i in 0..5_i64 {
            let result = c.update(&spy_bar(i, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10)));
            assert!(result.is_some(), "n=1 should emit on every bar (bar {i})");
        }
    }

    #[test]
    fn count_two_emits_every_other_bar() {
        let mut c = TradeBarConsolidator::new_count(2);
        assert!(c.update(&spy_bar(0, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_none());
        assert!(c.update(&spy_bar(1, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_some());
        assert!(c.update(&spy_bar(2, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_none());
        assert!(c.update(&spy_bar(3, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_some());
    }

    #[test]
    fn count_five_emits_on_fifth_bar_and_not_before() {
        let mut c = TradeBarConsolidator::new_count(5);
        for i in 0..4_i64 {
            let r = c.update(&spy_bar(i, dec!(10), dec!(11), dec!(9), dec!(10), dec!(100)));
            assert!(r.is_none(), "no emit before bar 5 (bar {i})");
        }
        let out = c.update(&spy_bar(4, dec!(10), dec!(11), dec!(9), dec!(10), dec!(100)));
        assert!(out.is_some(), "should emit on bar 5");
        assert_eq!(out.unwrap().volume, dec!(500));
    }

    #[test]
    fn count_n2_verify_full_ohlcv() {
        let mut c = TradeBarConsolidator::new_count(2);
        let b1 = spy_bar(0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(100));
        let b2 = spy_bar(1, dec!(11), dec!(14), dec!(10), dec!(13), dec!(200));

        assert!(c.update(&b1).is_none());
        let out = c.update(&b2).expect("emit on bar 2");

        assert_eq!(out.open, dec!(10));
        assert_eq!(out.high, dec!(14));
        assert_eq!(out.low, dec!(9));
        assert_eq!(out.close, dec!(13));
        assert_eq!(out.volume, dec!(300));
    }

    // ── Time-period mode ──────────────────────────────────────────────────────

    #[test]
    fn time_period_consolidator_no_emit_within_same_period() {
        let period = TimeSpan::from_mins(5);
        let mut c = TradeBarConsolidator::new_period(period.as_chrono_duration());

        let b0 = bar_at(spy(), 0, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10));
        let b4 = bar_at(spy(), 4 * 60_000_000_000, dec!(2), dec!(3), dec!(1.5), dec!(2), dec!(20));

        assert!(c.update(&b0).is_none(), "first bar in period should not emit");
        assert!(c.update(&b4).is_none(), "second bar in same period should not emit");
    }

    #[test]
    fn time_period_consolidator_emits_on_period_boundary_crossing() {
        let period = TimeSpan::from_mins(5);
        let mut c = TradeBarConsolidator::new_period(period.as_chrono_duration());

        let b0 = bar_at(spy(), 0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(50));
        let b5 = bar_at(spy(), 5 * 60_000_000_000, dec!(11), dec!(13), dec!(10), dec!(12), dec!(60));

        assert!(c.update(&b0).is_none());
        let out = c.update(&b5).expect("crossing into new period should emit previous period's bar");
        assert_eq!(out.open, dec!(10));
        assert_eq!(out.close, dec!(11));
    }

    #[test]
    fn time_period_consolidator_accumulates_ohlcv_across_period() {
        let period = TimeSpan::from_mins(5);
        let mut c = TradeBarConsolidator::new_period(period.as_chrono_duration());

        let b0 = bar_at(spy(), 0, dec!(10), dec!(15), dec!(9), dec!(12), dec!(100));
        let b3 = bar_at(spy(), 3 * 60_000_000_000, dec!(12), dec!(20), dec!(11), dec!(14), dec!(200));
        let b5 = bar_at(spy(), 5 * 60_000_000_000, dec!(14), dec!(16), dec!(13), dec!(15), dec!(50));

        assert!(c.update(&b0).is_none());
        assert!(c.update(&b3).is_none());
        let out = c.update(&b5).expect("should emit on new period");

        assert_eq!(out.open, dec!(10), "open = first bar's open");
        assert_eq!(out.high, dec!(20), "high = max across period");
        assert_eq!(out.low, dec!(9), "low = min across period");
        assert_eq!(out.close, dec!(14), "close = last bar's close in period");
        assert_eq!(out.volume, dec!(300), "volume = sum across period");
    }

    #[test]
    fn time_period_daily_consolidation_of_hourly_bars() {
        // 3 bars in day 0, then a bar in day 1 triggers emit of day 0.
        use chrono::Duration;
        let mut c = TradeBarConsolidator::new_period(Duration::days(1));

        // day 0 bars at hours 0, 1, 2 (nanos from Unix epoch)
        let h = 3_600_000_000_000_i64;
        let d = 86_400_000_000_000_i64;
        let b_h0 = bar_at(spy(), 0 * h, dec!(100), dec!(105), dec!(98), dec!(103), dec!(500));
        let b_h1 = bar_at(spy(), 1 * h, dec!(103), dec!(108), dec!(101), dec!(106), dec!(600));
        let b_h2 = bar_at(spy(), 2 * h, dec!(106), dec!(110), dec!(104), dec!(109), dec!(700));
        // day 1 bar triggers emit
        let b_d1 = bar_at(spy(), 1 * d, dec!(109), dec!(115), dec!(107), dec!(113), dec!(800));

        assert!(c.update(&b_h0).is_none());
        assert!(c.update(&b_h1).is_none());
        assert!(c.update(&b_h2).is_none());

        let emitted = c.update(&b_d1).expect("day boundary should emit");
        assert_eq!(emitted.open, dec!(100));
        assert_eq!(emitted.high, dec!(110));
        assert_eq!(emitted.low, dec!(98));
        assert_eq!(emitted.close, dec!(109));
        assert_eq!(emitted.volume, dec!(1800));
    }

    #[test]
    fn reset_clears_all_state() {
        let mut c = TradeBarConsolidator::new_count(3);
        c.update(&spy_bar(0, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10)));
        c.update(&spy_bar(1, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10)));
        c.reset();
        assert!(c.update(&spy_bar(2, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_none());
        assert!(c.update(&spy_bar(3, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10))).is_none());
        let out = c.update(&spy_bar(4, dec!(1), dec!(2), dec!(0.5), dec!(1), dec!(10)));
        assert!(out.is_some(), "should emit exactly 3 bars after reset");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// RenkoConsolidator
// ─────────────────────────────────────────────────────────────────────────────

mod renko_consolidator_tests {
    use super::*;

    /// Bar where every OHLC field equals `price`.
    fn price_bar(nanos: i64, close: rust_decimal::Decimal) -> TradeBar {
        bar_at(spy(), nanos, close, close, close, close, dec!(0))
    }

    const MIN: i64 = 60_000_000_000;

    #[test]
    fn first_bar_seeds_and_returns_none() {
        let mut r = RenkoConsolidator::new(dec!(1));
        assert!(r.update(&price_bar(0, dec!(10))).is_none(), "first tick should never emit");
    }

    #[test]
    fn no_emit_below_brick_size() {
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        assert!(r.update(&price_bar(MIN, dec!(10.9))).is_none(), "sub-brick move should not emit");
    }

    #[test]
    fn no_emit_exact_open_rate_no_move() {
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        assert!(r.update(&price_bar(MIN, dec!(10))).is_none(), "zero move should not emit");
    }

    #[test]
    fn emits_one_rising_brick_when_price_crosses_brick_size() {
        // Seed at 10 (snapped). Move to 11.1 > 10+1=11 → emits [10→11].
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        let result = r.update(&price_bar(MIN, dec!(11.1)));
        assert!(result.is_some(), "price move > brick_size should emit a brick");
        let brick = result.unwrap();
        assert_eq!(brick.open, dec!(10));
        assert_eq!(brick.close, dec!(11));
    }

    #[test]
    fn emits_one_falling_brick_when_price_drops_by_brick_size() {
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        // 8.9 < 10-1=9 → emits [10→9]
        let result = r.update(&price_bar(MIN, dec!(8.9)));
        assert!(result.is_some(), "downward move >= brick_size should emit a brick");
        let brick = result.unwrap();
        assert_eq!(brick.open, dec!(10));
        assert_eq!(brick.close, dec!(9));
    }

    #[test]
    fn multiple_bricks_on_large_upward_move() {
        // Seed at 10. Move to 13.1:
        //   iter1: limit=11, 13.1>11 → brick [10→11]
        //   iter2: limit=12, 13.1>12 → brick [11→12]
        //   iter3: limit=13, 13.1>13 → brick [12→13]
        //   iter4: limit=14, 13.1<=14 → stop  (total 3 bricks)
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        let first = r.update(&price_bar(MIN, dec!(13.1)));
        assert!(first.is_some(), "large move should emit at least one brick");
        let mut all = r.drain_pending();
        all.insert(0, first.unwrap());
        assert_eq!(all.len(), 3, "a >3× brick_size move should produce exactly 3 bricks");
        for (i, brick) in all.iter().enumerate() {
            let expected_open = dec!(10) + rust_decimal::Decimal::from(i as u64);
            let expected_close = expected_open + dec!(1);
            assert_eq!(brick.open, expected_open, "brick {i} open");
            assert_eq!(brick.close, expected_close, "brick {i} close");
        }
    }

    #[test]
    fn multiple_bricks_on_large_downward_move() {
        // Seed at 10. Move to 6.9:
        //   iter1: limit=9, 6.9<9 → brick [10→9]
        //   iter2: limit=8, 6.9<8 → brick [9→8]
        //   iter3: limit=7, 6.9<7 → brick [8→7]
        //   iter4: limit=6, 6.9>=6 → stop  (total 3 bricks)
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        let first = r.update(&price_bar(MIN, dec!(6.9)));
        assert!(first.is_some(), "large downward move should emit bricks");
        let mut all = r.drain_pending();
        all.insert(0, first.unwrap());
        assert_eq!(all.len(), 3, "large downward move should produce 3 bricks");
        assert_eq!(all[0].open, dec!(10));
        assert_eq!(all[0].close, dec!(9));
        assert_eq!(all[1].open, dec!(9));
        assert_eq!(all[1].close, dec!(8));
        assert_eq!(all[2].open, dec!(8));
        assert_eq!(all[2].close, dec!(7));
    }

    #[test]
    fn reversal_from_falling_to_rising() {
        // 1. Seed at 10.
        // 2. Fall: emit brick [10→9].
        // 3. Reversal: price > last_open(10)+brick_size(1)=11 → emit reversal [10→11].
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));

        let falling = r.update(&price_bar(MIN, dec!(8.9))).expect("falling brick");
        assert_eq!(falling.open, dec!(10));
        assert_eq!(falling.close, dec!(9));
        assert!(r.drain_pending().is_empty());

        // Price 11.1 > 10+1=11 triggers reversal
        let reversal = r.update(&price_bar(2 * MIN, dec!(11.1))).expect("reversal brick");
        assert_eq!(reversal.open, dec!(10), "reversal opens at last_open");
        assert_eq!(reversal.close, dec!(11), "reversal closes at last_open + brick_size");
    }

    #[test]
    fn reversal_from_rising_to_falling() {
        // 1. Seed at 10.
        // 2. Rise: emit brick [10→11].
        // 3. Reversal: price < last_open(10)-brick_size(1)=9 → emit reversal [10→9].
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));

        let rising = r.update(&price_bar(MIN, dec!(11.1))).expect("rising brick");
        assert_eq!(rising.open, dec!(10));
        assert_eq!(rising.close, dec!(11));
        assert!(r.drain_pending().is_empty());

        // Price 8.9 < 10-1=9 triggers reversal
        let reversal = r.update(&price_bar(2 * MIN, dec!(8.9))).expect("reversal brick");
        assert_eq!(reversal.open, dec!(10), "reversal opens at last_open");
        assert_eq!(reversal.close, dec!(9), "reversal closes at last_open - brick_size");
    }

    #[test]
    fn no_reversal_on_insufficient_pullback() {
        // After rising brick [10→11], price 9.5 is NOT < 10-1=9 → no reversal.
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        r.update(&price_bar(MIN, dec!(11.1)));
        r.drain_pending();

        assert!(
            r.update(&price_bar(2 * MIN, dec!(9.5))).is_none(),
            "insufficient pullback should not emit"
        );
    }

    #[test]
    fn get_closest_multiple_rounds_correctly() {
        // 103 / 10 → floor=10, modulus=3, round(0.3)=0 → 10*10=100
        assert_eq!(RenkoConsolidator::get_closest_multiple(dec!(103), dec!(10)), dec!(100));
        // 97 / 10 → floor=9, modulus=7, round(0.7)=1 → 10*10=100
        assert_eq!(RenkoConsolidator::get_closest_multiple(dec!(97), dec!(10)), dec!(100));
        // 10 / 5 → floor=2, modulus=0, round(0)=0 → 5*2=10
        assert_eq!(RenkoConsolidator::get_closest_multiple(dec!(10), dec!(5)), dec!(10));
        // 106 / 10 → floor=10, modulus=6, round(0.6)=1 → 10*11=110
        assert_eq!(RenkoConsolidator::get_closest_multiple(dec!(106), dec!(10)), dec!(110));
        // 104 / 10 → floor=10, modulus=4, round(0.4)=0 → 10*10=100
        assert_eq!(RenkoConsolidator::get_closest_multiple(dec!(104), dec!(10)), dec!(100));
        // 105 / 10 → floor=10, modulus=5, round(0.5)=0 (banker's: half rounds to even=0) → 10*10=100
        assert_eq!(RenkoConsolidator::get_closest_multiple(dec!(105), dec!(10)), dec!(100));
    }

    #[test]
    fn brick_high_gte_close_for_rising_brick() {
        let mut r = RenkoConsolidator::new(dec!(10));
        // Seed at 100
        r.update(&bar_at(spy(), 0, dec!(100), dec!(100), dec!(100), dec!(100), dec!(0)));
        // Bar with high=125, low=95, close=115 — produces brick 100→110
        let brick = r
            .update(&bar_at(spy(), MIN, dec!(115), dec!(125), dec!(95), dec!(115), dec!(0)))
            .expect("brick");
        assert_eq!(brick.open, dec!(100));
        assert_eq!(brick.close, dec!(110));
        assert!(brick.high >= dec!(110), "wicked high must be >= brick close");
    }

    #[test]
    fn reset_clears_renko_state() {
        let mut r = RenkoConsolidator::new(dec!(1));
        r.update(&price_bar(0, dec!(10)));
        r.update(&price_bar(MIN, dec!(11.1)));
        r.reset();
        assert!(
            r.update(&price_bar(2 * MIN, dec!(20))).is_none(),
            "reset should re-seed on next tick"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HeikinAshiConsolidator
// ─────────────────────────────────────────────────────────────────────────────

mod heikin_ashi_consolidator_tests {
    use super::*;

    #[test]
    fn first_bar_ha_close_is_average_ohlc() {
        // HA_Close = (O + H + L + C) / 4
        let mut ha = HeikinAshiConsolidator::new();
        let b = bar_at(spy(), 0, dec!(10), dec!(20), dec!(5), dec!(15), dec!(100));
        let out = ha.update(&b).expect("HA always emits");
        // (10 + 20 + 5 + 15) / 4 = 12.5
        assert_eq!(out.close, dec!(12.5), "HA close = (O+H+L+C)/4");
    }

    #[test]
    fn first_bar_ha_open_is_average_oc() {
        // HA_Open (first bar) = (O + C) / 2
        let mut ha = HeikinAshiConsolidator::new();
        let b = bar_at(spy(), 0, dec!(10), dec!(20), dec!(5), dec!(14), dec!(100));
        let out = ha.update(&b).expect("HA always emits");
        // (10 + 14) / 2 = 12
        assert_eq!(out.open, dec!(12), "first bar HA open = (O+C)/2");
    }

    #[test]
    fn first_bar_ha_high_is_max_of_high_ha_open_ha_close() {
        let mut ha = HeikinAshiConsolidator::new();
        // O=10, H=20, L=5, C=15 → HA_Close=12.5, HA_Open=12.5
        let b = bar_at(spy(), 0, dec!(10), dec!(20), dec!(5), dec!(15), dec!(100));
        let out = ha.update(&b).expect("HA always emits");
        // max(20, 12.5, 12.5) = 20
        assert_eq!(out.high, dec!(20), "HA high = max(H, HA_Open, HA_Close)");
    }

    #[test]
    fn first_bar_ha_low_is_min_of_low_ha_open_ha_close() {
        let mut ha = HeikinAshiConsolidator::new();
        // O=10, H=20, L=5, C=15 → HA_Close=12.5, HA_Open=12.5
        let b = bar_at(spy(), 0, dec!(10), dec!(20), dec!(5), dec!(15), dec!(100));
        let out = ha.update(&b).expect("HA always emits");
        // min(5, 12.5, 12.5) = 5
        assert_eq!(out.low, dec!(5), "HA low = min(L, HA_Open, HA_Close)");
    }

    #[test]
    fn second_bar_ha_open_uses_previous_ha_values() {
        // HA_Open(n) = (prev_HA_Open + prev_HA_Close) / 2
        let mut ha = HeikinAshiConsolidator::new();
        // Bar 1: O=10, H=20, L=5, C=15 → HA_Close=12.5, HA_Open=12.5
        let b1 = bar_at(spy(), 0, dec!(10), dec!(20), dec!(5), dec!(15), dec!(100));
        ha.update(&b1).expect("HA always emits");

        // Bar 2
        let b2 = bar_at(spy(), 60_000_000_000, dec!(15), dec!(25), dec!(12), dec!(20), dec!(200));
        let out2 = ha.update(&b2).expect("HA always emits");

        // prev_HA_Open=12.5, prev_HA_Close=12.5 → HA_Open2 = (12.5+12.5)/2 = 12.5
        assert_eq!(out2.open, dec!(12.5), "second bar HA open = (prev_HA_Open + prev_HA_Close)/2");
        // HA_Close2 = (15+25+12+20)/4 = 72/4 = 18
        assert_eq!(out2.close, dec!(18), "second bar HA close = (O+H+L+C)/4");
    }

    #[test]
    fn third_bar_ha_open_smoothing_chain() {
        let mut ha = HeikinAshiConsolidator::new();
        let b1 = bar_at(spy(), 0, dec!(100), dec!(110), dec!(95), dec!(105), dec!(1000));
        let b2 = bar_at(spy(), 60_000_000_000, dec!(105), dec!(115), dec!(100), dec!(110), dec!(1200));
        let b3 = bar_at(spy(), 120_000_000_000, dec!(110), dec!(120), dec!(108), dec!(118), dec!(900));

        let ha1 = ha.update(&b1).expect("HA1");
        let ha2 = ha.update(&b2).expect("HA2");
        let ha3 = ha.update(&b3).expect("HA3");

        // ha3.open = (ha2.open + ha2.close) / 2
        let expected_ha3_open = (ha2.open + ha2.close) / dec!(2);
        assert_eq!(ha3.open, expected_ha3_open, "ha3 open = (ha2.open + ha2.close)/2");

        // ha3.close = (110+120+108+118)/4 = 114
        assert_eq!(ha3.close, dec!(114));

        // Verify HA High/Low invariants on ha2 (using actual ha2 values from ha1's output)
        let _ = ha1; // ensure it was computed
        assert!(ha2.high >= ha2.open && ha2.high >= ha2.close, "HA_High >= both opens/closes");
        assert!(ha2.low <= ha2.open && ha2.low <= ha2.close, "HA_Low <= both opens/closes");
    }

    #[test]
    fn ha_emits_on_every_update() {
        let mut ha = HeikinAshiConsolidator::new();
        for i in 0..5_i64 {
            let b = bar_at(spy(), i * 60_000_000_000, dec!(10), dec!(12), dec!(9), dec!(11), dec!(50));
            assert!(ha.update(&b).is_some(), "HA should emit on every bar (bar {i})");
        }
    }

    #[test]
    fn ha_preserves_volume_and_symbol() {
        let mut ha = HeikinAshiConsolidator::new();
        let b = bar_at(spy(), 0, dec!(10), dec!(12), dec!(8), dec!(11), dec!(999));
        let out = ha.update(&b).expect("HA emits");
        assert_eq!(out.volume, dec!(999), "volume is passed through unchanged");
        assert_eq!(out.symbol, spy());
    }

    #[test]
    fn ha_reset_restores_first_bar_seed_behavior() {
        let mut ha = HeikinAshiConsolidator::new();
        let b = bar_at(spy(), 0, dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));
        let ha1 = ha.update(&b).expect("first bar HA");
        let open_before_reset = ha1.open;

        ha.reset();

        // After reset, same input should produce identical output
        let b2 = bar_at(spy(), 60_000_000_000, dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000));
        let ha_after = ha.update(&b2).expect("HA after reset");
        assert_eq!(ha_after.open, open_before_reset, "reset restores first-bar seed formula");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// VolumeConsolidator
// ─────────────────────────────────────────────────────────────────────────────

mod volume_consolidator_tests {
    use super::*;

    #[test]
    fn no_emit_before_threshold_reached() {
        let mut v = VolumeConsolidator::new(dec!(500));
        let b1 = spy_bar(0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(200));
        let b2 = spy_bar(1, dec!(11), dec!(13), dec!(10), dec!(12), dec!(200));
        assert!(v.update(&b1).is_none(), "accumulated volume < threshold → no emit");
        assert!(v.update(&b2).is_none(), "accumulated volume still < threshold → no emit");
    }

    #[test]
    fn emits_when_volume_meets_threshold() {
        let mut v = VolumeConsolidator::new(dec!(300));
        let b1 = spy_bar(0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(100));
        let b2 = spy_bar(1, dec!(11), dec!(13), dec!(10), dec!(12), dec!(100));
        let b3 = spy_bar(2, dec!(12), dec!(14), dec!(11), dec!(13), dec!(100));
        assert!(v.update(&b1).is_none());
        assert!(v.update(&b2).is_none());
        assert!(v.update(&b3).is_some(), "should emit when accumulated volume reaches threshold");
    }

    #[test]
    fn consolidated_volume_equals_sum() {
        let mut v = VolumeConsolidator::new(dec!(300));
        let b1 = spy_bar(0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(100));
        let b2 = spy_bar(1, dec!(11), dec!(13), dec!(10), dec!(12), dec!(100));
        let b3 = spy_bar(2, dec!(12), dec!(14), dec!(11), dec!(13), dec!(100));
        v.update(&b1);
        v.update(&b2);
        let out = v.update(&b3).expect("should emit");
        assert_eq!(out.volume, dec!(300), "consolidated volume = sum");
    }

    #[test]
    fn consolidated_high_low_open_close_aggregated_correctly() {
        let mut v = VolumeConsolidator::new(dec!(300));
        let b1 = spy_bar(0, dec!(10), dec!(20), dec!(8), dec!(15), dec!(100));
        let b2 = spy_bar(1, dec!(15), dec!(18), dec!(12), dec!(16), dec!(100));
        let b3 = spy_bar(2, dec!(16), dec!(25), dec!(14), dec!(22), dec!(100));
        v.update(&b1);
        v.update(&b2);
        let out = v.update(&b3).expect("should emit");
        assert_eq!(out.open, dec!(10), "open = first bar's open");
        assert_eq!(out.high, dec!(25), "high = max across all bars");
        assert_eq!(out.low, dec!(8), "low = min across all bars");
        assert_eq!(out.close, dec!(22), "close = last bar's close");
    }

    #[test]
    fn volume_consolidator_resets_and_emits_again() {
        let mut v = VolumeConsolidator::new(dec!(200));
        let b1 = spy_bar(0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(100));
        let b2 = spy_bar(1, dec!(11), dec!(13), dec!(10), dec!(12), dec!(100));
        v.update(&b1);
        assert!(v.update(&b2).is_some(), "should emit at 200 volume");

        let b3 = spy_bar(2, dec!(12), dec!(14), dec!(11), dec!(13), dec!(100));
        let b4 = spy_bar(3, dec!(13), dec!(15), dec!(12), dec!(14), dec!(100));
        assert!(v.update(&b3).is_none(), "no emit mid-window");
        assert!(v.update(&b4).is_some(), "should emit again after second 200-volume window");
    }

    #[test]
    fn single_bar_exceeding_threshold_emits_immediately() {
        let mut v = VolumeConsolidator::new(dec!(100));
        let b = spy_bar(0, dec!(10), dec!(12), dec!(9), dec!(11), dec!(500));
        let out = v.update(&b).expect("single bar volume >= threshold should emit immediately");
        assert_eq!(out.volume, dec!(500));
    }

    #[test]
    fn partial_volume_then_reset_starts_fresh() {
        let mut v = VolumeConsolidator::new(dec!(1000));
        let b1 = spy_bar(0, dec!(10), dec!(11), dec!(9), dec!(10), dec!(400));
        v.update(&b1);
        v.reset();

        let b2 = spy_bar(1, dec!(10), dec!(11), dec!(9), dec!(10), dec!(400));
        assert!(v.update(&b2).is_none(), "no emit after reset with partial volume");
        let b3 = spy_bar(2, dec!(10), dec!(11), dec!(9), dec!(10), dec!(600));
        let out = v.update(&b3).expect("should emit after accumulating 1000");
        assert_eq!(out.volume, dec!(1000));
    }

    #[test]
    fn two_bars_exactly_at_threshold() {
        let mut v = VolumeConsolidator::new(dec!(1000));
        let b1 = spy_bar(0, dec!(10), dec!(11), dec!(9), dec!(10), dec!(600));
        let b2 = spy_bar(1, dec!(10), dec!(12), dec!(9), dec!(11), dec!(400));

        assert!(v.update(&b1).is_none());
        let out = v.update(&b2).expect("600+400=1000 should emit");
        assert_eq!(out.volume, dec!(1000));
        assert_eq!(out.open, dec!(10));
        assert_eq!(out.high, dec!(12));
        assert_eq!(out.low, dec!(9));
        assert_eq!(out.close, dec!(11));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CalendarConsolidator — Daily
// ─────────────────────────────────────────────────────────────────────────────

mod calendar_daily_tests {
    use super::*;

    // 2020-01-02T00:00:00Z = 1577923200 seconds
    const JAN2_2020: i64 = 1_577_923_200_i64 * 1_000_000_000;
    const JAN3_2020: i64 = JAN2_2020 + 86_400_000_000_000;
    const JAN4_2020: i64 = JAN3_2020 + 86_400_000_000_000;
    const ONE_HOUR_NS: i64 = 3_600_000_000_000;

    #[test]
    fn no_emit_within_same_day() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Daily);
        let b1 = daily_bar_at(JAN2_2020, dec!(100), dec!(105), dec!(98), dec!(103), dec!(500));
        let b2 = daily_bar_at(JAN2_2020 + ONE_HOUR_NS, dec!(103), dec!(108), dec!(100), dec!(106), dec!(600));
        assert!(c.update(&b1).is_none());
        assert!(c.update(&b2).is_none(), "same day should not emit");
    }

    #[test]
    fn emits_at_day_boundary() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Daily);
        let b1 = daily_bar_at(JAN2_2020, dec!(100), dec!(105), dec!(98), dec!(103), dec!(500));
        let b2 = daily_bar_at(JAN2_2020 + ONE_HOUR_NS, dec!(103), dec!(108), dec!(100), dec!(106), dec!(600));
        let b3 = daily_bar_at(JAN3_2020, dec!(106), dec!(110), dec!(104), dec!(109), dec!(700));

        c.update(&b1);
        c.update(&b2);
        let emitted = c.update(&b3).expect("new day triggers emit");

        assert_eq!(emitted.open, dec!(100));
        assert_eq!(emitted.high, dec!(108));
        assert_eq!(emitted.low, dec!(98));
        assert_eq!(emitted.close, dec!(106));
        assert_eq!(emitted.volume, dec!(1100));
    }

    #[test]
    fn three_days_emit_two_bars() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Daily);

        let d1 = daily_bar_at(JAN2_2020, dec!(100), dec!(110), dec!(95), dec!(108), dec!(1000));
        let d2a = daily_bar_at(JAN3_2020, dec!(108), dec!(115), dec!(105), dec!(113), dec!(1200));
        let d2b = daily_bar_at(JAN3_2020 + ONE_HOUR_NS, dec!(113), dec!(118), dec!(110), dec!(116), dec!(800));
        let d3 = daily_bar_at(JAN4_2020, dec!(116), dec!(120), dec!(112), dec!(118), dec!(900));

        c.update(&d1);
        let e1 = c.update(&d2a).expect("day 2 starts — emit day 1");
        assert_eq!(e1.volume, dec!(1000));

        c.update(&d2b);
        let e2 = c.update(&d3).expect("day 3 starts — emit day 2");
        assert_eq!(e2.volume, dec!(2000), "two bars in day 2");
        assert_eq!(e2.open, dec!(108));
        assert_eq!(e2.close, dec!(116));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CalendarConsolidator — Monthly
// ─────────────────────────────────────────────────────────────────────────────

mod calendar_monthly_tests {
    use super::*;

    // 2020-01-02T00:00:00Z
    const JAN2_2020: i64 = 1_577_923_200_i64 * 1_000_000_000;
    // 2020-01-15T00:00:00Z = 1578960000 seconds
    const JAN15_2020: i64 = 1_578_960_000_i64 * 1_000_000_000;
    // 2020-02-01T00:00:00Z = 1580515200 seconds
    const FEB1_2020: i64 = 1_580_515_200_i64 * 1_000_000_000;
    // 2020-02-15T00:00:00Z
    const FEB15_2020: i64 = FEB1_2020 + 14 * 86_400_000_000_000_i64;
    // 2020-03-01T00:00:00Z = 1583020800 seconds
    const MAR1_2020: i64 = 1_583_020_800_i64 * 1_000_000_000;

    #[test]
    fn no_emit_within_same_month() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Monthly);
        let b1 = daily_bar_at(JAN2_2020, dec!(100), dec!(105), dec!(98), dec!(103), dec!(1000));
        let b2 = daily_bar_at(JAN15_2020, dec!(103), dec!(108), dec!(101), dec!(106), dec!(1200));
        assert!(c.update(&b1).is_none());
        assert!(c.update(&b2).is_none(), "same month should not emit");
    }

    #[test]
    fn emits_when_month_changes() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Monthly);
        let b_jan1 = daily_bar_at(JAN2_2020, dec!(100), dec!(105), dec!(98), dec!(103), dec!(1000));
        let b_jan2 = daily_bar_at(JAN15_2020, dec!(103), dec!(108), dec!(101), dec!(106), dec!(1200));
        let b_feb = daily_bar_at(FEB1_2020, dec!(106), dec!(112), dec!(104), dec!(110), dec!(1500));

        c.update(&b_jan1);
        c.update(&b_jan2);
        let emitted = c.update(&b_feb).expect("February bar should emit January bar");

        assert_eq!(emitted.open, dec!(100));
        assert_eq!(emitted.high, dec!(108));
        assert_eq!(emitted.low, dec!(98));
        assert_eq!(emitted.close, dec!(106));
        assert_eq!(emitted.volume, dec!(2200));
    }

    #[test]
    fn two_months_emit_two_bars() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Monthly);

        let jan = daily_bar_at(JAN2_2020, dec!(100), dec!(110), dec!(95), dec!(108), dec!(5000));
        let feb_a = daily_bar_at(FEB1_2020, dec!(108), dec!(115), dec!(105), dec!(112), dec!(6000));
        let feb_b = daily_bar_at(FEB15_2020, dec!(112), dec!(120), dec!(108), dec!(118), dec!(4000));
        let mar = daily_bar_at(MAR1_2020, dec!(118), dec!(125), dec!(115), dec!(122), dec!(7000));

        c.update(&jan);
        let e1 = c.update(&feb_a).expect("feb triggers jan emit");
        assert_eq!(e1.volume, dec!(5000));

        c.update(&feb_b);
        let e2 = c.update(&mar).expect("mar triggers feb emit");
        assert_eq!(e2.volume, dec!(10000), "two feb bars combined");
        assert_eq!(e2.open, dec!(108));
        assert_eq!(e2.close, dec!(118));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// CalendarConsolidator — Weekly
// ─────────────────────────────────────────────────────────────────────────────

mod calendar_weekly_tests {
    use super::*;

    // 2020-01-06 (Mon, ISO week 2 of 2020) at 00:00:00 UTC = 1578268800 seconds
    const WEEK2_MON: i64 = 1_578_268_800_i64 * 1_000_000_000;
    const WEEK2_TUE: i64 = WEEK2_MON + 86_400_000_000_000;
    const WEEK3_MON: i64 = WEEK2_MON + 7 * 86_400_000_000_000_i64;

    #[test]
    fn no_emit_within_same_week() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Weekly);
        let b1 = daily_bar_at(WEEK2_MON, dec!(100), dec!(105), dec!(98), dec!(103), dec!(1000));
        let b2 = daily_bar_at(WEEK2_TUE, dec!(103), dec!(108), dec!(100), dec!(106), dec!(1200));
        assert!(c.update(&b1).is_none());
        assert!(c.update(&b2).is_none(), "same week should not emit");
    }

    #[test]
    fn emits_at_week_boundary() {
        let mut c = CalendarConsolidator::new(CalendarPeriod::Weekly);

        let b1 = daily_bar_at(WEEK2_MON, dec!(100), dec!(105), dec!(98), dec!(103), dec!(1000));
        let b2 = daily_bar_at(WEEK2_TUE, dec!(103), dec!(107), dec!(100), dec!(106), dec!(1200));
        let b3 = daily_bar_at(WEEK3_MON, dec!(106), dec!(110), dec!(104), dec!(108), dec!(900));

        c.update(&b1);
        c.update(&b2);
        let emitted = c.update(&b3).expect("new week should emit prior week bar");

        assert_eq!(emitted.open, dec!(100));
        assert_eq!(emitted.high, dec!(107));
        assert_eq!(emitted.low, dec!(98));
        assert_eq!(emitted.close, dec!(106));
        assert_eq!(emitted.volume, dec!(2200));
    }
}
