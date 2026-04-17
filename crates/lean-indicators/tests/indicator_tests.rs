use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::TradeBar;
/// Comprehensive indicator tests — mirrors LEAN C# unit test suite.
///
/// Covers: ADX, Stochastic, ROC, CCI, WilliamsR, DonchianChannel,
///         KeltnerChannel, VWAP, OBV, MFI, Aroon, and cross-cutting
///         readiness / reset / bounds checks for all indicators.
use lean_indicators::{
    cci::Cci, indicator::Indicator, williams_r::WilliamsR, Adx, Aroon, BollingerBands,
    DonchianChannel, Ema, KeltnerChannel, MoneyFlowIndex, Obv, Roc, Rsi, Sma, Stochastic, Vwap,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn make_bar(
    i: i64,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
    volume: Decimal,
) -> TradeBar {
    TradeBar {
        symbol: spy(),
        time: ts(i),
        end_time: ts(i + 1),
        open,
        high,
        low,
        close,
        volume,
        period: TimeSpan::ONE_DAY,
    }
}

/// Convenience: build a bar with O = midpoint(H,L).
fn bar(i: i64, high: Decimal, low: Decimal, close: Decimal) -> TradeBar {
    make_bar(i, (high + low) / dec!(2), high, low, close, dec!(1_000_000))
}

fn assert_approx(a: Decimal, b: Decimal, tol: Decimal, msg: &str) {
    assert!(
        (a - b).abs() <= tol,
        "{}: expected {} ≈ {} (diff = {})",
        msg,
        a,
        b,
        (a - b).abs()
    );
}

// ─── ADX Tests ────────────────────────────────────────────────────────────────

#[test]
fn adx_not_ready_before_warmup() {
    // ADX only increments `samples` on bars 2+ (needs prev high/low/close).
    // For period=5, is_ready requires samples >= 10.
    // samples=10 is reached after 11 total bars (bar 1 doesn't increment).
    let mut adx = Adx::new(5);
    assert!(!adx.is_ready());
    for i in 0..10 {
        adx.update_bar(&bar(i, dec!(105), dec!(95), dec!(100)));
        assert!(
            !adx.is_ready(),
            "ADX should not be ready after {} bars",
            i + 1
        );
    }
    adx.update_bar(&bar(10, dec!(105), dec!(95), dec!(100)));
    assert!(
        adx.is_ready(),
        "ADX should be ready after 11 bars (1 seed + period*2 samples)"
    );
}

#[test]
fn adx_warm_up_period_is_period_times_two() {
    let adx = Adx::new(14);
    assert_eq!(adx.warm_up_period(), 28);
}

#[test]
fn adx_trending_up_produces_positive_plus_di() {
    // Steadily rising prices → +DI > -DI
    let mut adx = Adx::new(5);
    for i in 0..15 {
        let base = Decimal::from(100 + i * 2);
        adx.update_bar(&bar(
            i as i64,
            base + dec!(3),
            base - dec!(1),
            base + dec!(2),
        ));
    }
    assert!(adx.is_ready());
    assert!(
        adx.plus_di > adx.minus_di,
        "+DI ({}) should exceed -DI ({}) in uptrend",
        adx.plus_di,
        adx.minus_di
    );
}

#[test]
fn adx_trending_down_produces_positive_minus_di() {
    // Steadily falling prices → -DI > +DI
    let mut adx = Adx::new(5);
    for i in 0..15 {
        let base = Decimal::from(200 - i * 2);
        adx.update_bar(&bar(
            i as i64,
            base + dec!(1),
            base - dec!(3),
            base - dec!(2),
        ));
    }
    assert!(adx.is_ready());
    assert!(
        adx.minus_di > adx.plus_di,
        "-DI ({}) should exceed +DI ({}) in downtrend",
        adx.minus_di,
        adx.plus_di
    );
}

#[test]
fn adx_value_bounded_0_to_100() {
    let mut adx = Adx::new(5);
    for i in 0..30 {
        let h = Decimal::from(100 + (i % 7) * 3);
        let l = h - dec!(8);
        let c = (h + l) / dec!(2);
        let result = adx.update_bar(&bar(i as i64, h, l, c));
        if result.is_ready() {
            assert!(
                result.value >= dec!(0) && result.value <= dec!(100),
                "ADX out of bounds: {}",
                result.value
            );
        }
    }
}

#[test]
fn adx_ranging_market_below_25() {
    // Alternating up/down with small ranges → ADX should stay relatively low
    let mut adx = Adx::new(5);
    let prices = [
        100i32, 101, 100, 99, 100, 101, 100, 99, 100, 101, 100, 99, 100, 101, 100, 99, 100, 101,
        100, 99,
    ];
    for (i, &p) in prices.iter().enumerate() {
        let c = Decimal::from(p);
        adx.update_bar(&bar(i as i64, c + dec!(1), c - dec!(1), c));
    }
    assert!(adx.is_ready());
    // A choppy market should produce low ADX (below 25 is the classic threshold)
    assert!(
        adx.current().value < dec!(25),
        "ADX ({}) should be < 25 in a ranging market",
        adx.current().value
    );
}

#[test]
fn adx_reset_clears_state() {
    let mut adx = Adx::new(5);
    for i in 0..15 {
        adx.update_bar(&bar(i, dec!(105), dec!(95), dec!(100)));
    }
    assert!(adx.is_ready());

    adx.reset();

    assert!(!adx.is_ready());
    assert_eq!(adx.samples(), 0);
    assert!(!adx.current().is_ready());
}

// ─── Stochastic Tests ─────────────────────────────────────────────────────────

#[test]
fn stochastic_not_ready_before_warmup() {
    // warm_up_period = k_period + d_period - 1 = 14 + 3 - 1 = 16
    let mut stoch = Stochastic::new(14, 3);
    assert!(!stoch.is_ready());
    for i in 0..15 {
        stoch.update_bar(&bar(i, dec!(105), dec!(95), dec!(100)));
        assert!(
            !stoch.is_ready(),
            "Stochastic should not be ready after {} bars",
            i + 1
        );
    }
    stoch.update_bar(&bar(15, dec!(105), dec!(95), dec!(100)));
    assert!(stoch.is_ready());
}

#[test]
fn stochastic_warm_up_period() {
    let s = Stochastic::new(14, 3);
    assert_eq!(s.warm_up_period(), 16); // 14 + 3 - 1
}

#[test]
fn stochastic_k_overbought_close_at_high() {
    // Close always at high → %K = 100
    let mut stoch = Stochastic::new(5, 3);
    for i in 0..10 {
        // close = high = 110, low = 90 → K = (110-90)/(110-90) * 100 = 100
        let result = stoch.update_bar(&bar(i, dec!(110), dec!(90), dec!(110)));
        if result.is_ready() {
            assert_eq!(stoch.k, dec!(100), "%K should be 100 when close = high");
        }
    }
    assert!(stoch.is_ready());
}

#[test]
fn stochastic_k_oversold_close_at_low() {
    // Close always at low → %K = 0
    let mut stoch = Stochastic::new(5, 3);
    for i in 0..10 {
        let result = stoch.update_bar(&bar(i, dec!(110), dec!(90), dec!(90)));
        if result.is_ready() {
            assert_eq!(stoch.k, dec!(0), "%K should be 0 when close = low");
        }
    }
    assert!(stoch.is_ready());
}

#[test]
fn stochastic_k_bounded_0_to_100() {
    let mut stoch = Stochastic::new(5, 3);
    let closes = [100, 102, 98, 104, 96, 108, 94, 106, 100, 103, 97, 105];
    for (i, &c) in closes.iter().enumerate() {
        let cv = Decimal::from(c);
        let result = stoch.update_bar(&bar(i as i64, cv + dec!(3), cv - dec!(3), cv));
        if result.is_ready() {
            assert!(
                stoch.k >= dec!(0) && stoch.k <= dec!(100),
                "%K out of bounds: {}",
                stoch.k
            );
        }
    }
}

#[test]
fn stochastic_d_is_sma_of_k() {
    // D should be close to K when K is constant
    let mut stoch = Stochastic::new(3, 3);
    // All closes at midpoint → K = 50
    for i in 0..10 {
        stoch.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(stoch.is_ready());
    // K = 50 since close = (110+90)/2 = 100, range = 20, K = (100-90)/20*100 = 50
    assert_approx(stoch.k, dec!(50), dec!(0.001), "K should be 50 at midpoint");
    assert_approx(
        stoch.d,
        dec!(50),
        dec!(0.001),
        "D should equal K when K is constant",
    );
}

#[test]
fn stochastic_reset_clears_state() {
    let mut stoch = Stochastic::new(5, 3);
    for i in 0..10 {
        stoch.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(stoch.is_ready());

    stoch.reset();

    assert!(!stoch.is_ready());
    assert_eq!(stoch.samples(), 0);
}

// ─── ROC Tests ────────────────────────────────────────────────────────────────

#[test]
fn roc_not_ready_before_period_plus_one() {
    let mut roc = Roc::new(3);
    assert!(!roc.is_ready());
    for i in 0..3 {
        roc.update_price(ts(i), dec!(100));
        assert!(!roc.is_ready());
    }
    roc.update_price(ts(3), dec!(100));
    assert!(roc.is_ready());
}

#[test]
fn roc_warm_up_period() {
    let roc = Roc::new(10);
    assert_eq!(roc.warm_up_period(), 11);
}

#[test]
fn roc_correct_percent_change() {
    let mut roc = Roc::new(1);
    // period=1: ROC = (current - prev) / prev * 100
    roc.update_price(ts(0), dec!(100));
    let r = roc.update_price(ts(1), dec!(110));
    assert!(r.is_ready());
    // ROC = (110 - 100) / 100 * 100 = 10%
    assert_approx(r.value, dec!(10), dec!(0.001), "ROC should be 10%");
}

#[test]
fn roc_double_period_correct() {
    let mut roc = Roc::new(2);
    roc.update_price(ts(0), dec!(100));
    roc.update_price(ts(1), dec!(105));
    let r = roc.update_price(ts(2), dec!(110));
    assert!(r.is_ready());
    // ROC = (110 - 100) / 100 * 100 = 10%
    assert_approx(r.value, dec!(10), dec!(0.001), "ROC(2) should be 10%");
}

#[test]
fn roc_negative_for_declining_price() {
    let mut roc = Roc::new(1);
    roc.update_price(ts(0), dec!(100));
    let r = roc.update_price(ts(1), dec!(90));
    assert!(r.is_ready());
    // ROC = (90 - 100) / 100 * 100 = -10%
    assert_approx(r.value, dec!(-10), dec!(0.001), "ROC should be -10%");
}

#[test]
fn roc_zero_for_flat_price() {
    let mut roc = Roc::new(3);
    for i in 0..4 {
        roc.update_price(ts(i), dec!(100));
    }
    assert!(roc.is_ready());
    assert_eq!(roc.current().value, dec!(0));
}

#[test]
fn roc_reset_clears_state() {
    let mut roc = Roc::new(3);
    for i in 0..5 {
        roc.update_price(ts(i), Decimal::from(100 + i));
    }
    assert!(roc.is_ready());

    roc.reset();

    assert!(!roc.is_ready());
    assert_eq!(roc.samples(), 0);
}

// ─── CCI Tests ────────────────────────────────────────────────────────────────

#[test]
fn cci_not_ready_before_period() {
    let mut cci = Cci::new(5);
    assert!(!cci.is_ready());
    for i in 0..4 {
        cci.update_bar(&bar(i, dec!(105), dec!(95), dec!(100)));
        assert!(!cci.is_ready());
    }
    cci.update_bar(&bar(4, dec!(105), dec!(95), dec!(100)));
    assert!(cci.is_ready());
}

#[test]
fn cci_warm_up_period_equals_period() {
    let cci = Cci::new(20);
    assert_eq!(cci.warm_up_period(), 20);
}

#[test]
fn cci_flat_prices_gives_zero() {
    let mut cci = Cci::new(5);
    // Identical bars → mean_dev = 0 → CCI = 0
    for i in 0..5 {
        cci.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(cci.is_ready());
    assert_eq!(cci.current().value, dec!(0));
}

#[test]
fn cci_overbought_for_rising_prices() {
    // CCI > 100 indicates overbought
    let mut cci = Cci::new(5);
    // Sharply rising typical prices
    for i in 0..5 {
        let c = Decimal::from(100 + i * 10);
        cci.update_bar(&bar(i as i64, c + dec!(2), c - dec!(2), c));
    }
    assert!(cci.is_ready());
    // Most recent typical is much higher than mean → CCI should be positive and large
    assert!(
        cci.current().value > dec!(0),
        "CCI should be positive for rising prices, got {}",
        cci.current().value
    );
}

#[test]
fn cci_negative_for_falling_prices() {
    let mut cci = Cci::new(5);
    for i in 0..5 {
        let c = Decimal::from(100 - i * 10);
        cci.update_bar(&bar(i as i64, c + dec!(2), c - dec!(2), c));
    }
    assert!(cci.is_ready());
    assert!(
        cci.current().value < dec!(0),
        "CCI should be negative for falling prices, got {}",
        cci.current().value
    );
}

#[test]
fn cci_produces_reasonable_values() {
    let mut cci = Cci::new(10);
    let prices = [
        100, 102, 98, 103, 97, 105, 95, 108, 92, 110, 100, 102, 98, 103, 97,
    ];
    for (i, &p) in prices.iter().enumerate() {
        let c = Decimal::from(p);
        let r = cci.update_bar(&bar(i as i64, c + dec!(3), c - dec!(3), c));
        if r.is_ready() {
            // CCI values beyond ±300 are uncommon
            assert!(
                r.value >= dec!(-500) && r.value <= dec!(500),
                "CCI value {} seems unreasonably large",
                r.value
            );
        }
    }
}

// ─── WilliamsR Tests ──────────────────────────────────────────────────────────

#[test]
fn williams_r_not_ready_before_period() {
    let mut wr = WilliamsR::new(5);
    assert!(!wr.is_ready());
    for i in 0..4 {
        wr.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
        assert!(!wr.is_ready());
    }
    wr.update_bar(&bar(4, dec!(110), dec!(90), dec!(100)));
    assert!(wr.is_ready());
}

#[test]
fn williams_r_warm_up_period_equals_period() {
    let wr = WilliamsR::new(14);
    assert_eq!(wr.warm_up_period(), 14);
}

#[test]
fn williams_r_close_at_high_gives_zero() {
    // Close = highest high → W%R = -100 * (H - H) / (H - L) = 0
    let mut wr = WilliamsR::new(5);
    for i in 0..5 {
        let r = wr.update_bar(&bar(i, dec!(110), dec!(90), dec!(110)));
        if r.is_ready() {
            assert_eq!(r.value, dec!(0), "W%R should be 0 when close = high");
        }
    }
}

#[test]
fn williams_r_close_at_low_gives_minus_100() {
    // Close = lowest low → W%R = -100 * (H - L) / (H - L) = -100
    let mut wr = WilliamsR::new(5);
    for i in 0..5 {
        let r = wr.update_bar(&bar(i, dec!(110), dec!(90), dec!(90)));
        if r.is_ready() {
            assert_eq!(r.value, dec!(-100), "W%R should be -100 when close = low");
        }
    }
}

#[test]
fn williams_r_bounded_minus100_to_zero() {
    let mut wr = WilliamsR::new(5);
    let closes = [100, 102, 98, 104, 96, 108, 94, 106, 100, 103];
    for (i, &c) in closes.iter().enumerate() {
        let cv = Decimal::from(c);
        let r = wr.update_bar(&bar(i as i64, cv + dec!(5), cv - dec!(5), cv));
        if r.is_ready() {
            assert!(
                r.value >= dec!(-100) && r.value <= dec!(0),
                "W%R out of bounds: {}",
                r.value
            );
        }
    }
}

#[test]
fn williams_r_reset_clears_state() {
    let mut wr = WilliamsR::new(5);
    for i in 0..8 {
        wr.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(wr.is_ready());

    wr.reset();

    assert!(!wr.is_ready());
    assert_eq!(wr.samples(), 0);
}

// ─── Donchian Channel Tests ───────────────────────────────────────────────────

#[test]
fn donchian_not_ready_before_period() {
    let mut dc = DonchianChannel::new(5);
    assert!(!dc.is_ready());
    for i in 0..4 {
        dc.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
        assert!(!dc.is_ready());
    }
    dc.update_bar(&bar(4, dec!(110), dec!(90), dec!(100)));
    assert!(dc.is_ready());
}

#[test]
fn donchian_warm_up_period_equals_period() {
    let dc = DonchianChannel::new(20);
    assert_eq!(dc.warm_up_period(), 20);
}

#[test]
fn donchian_upper_above_lower() {
    let mut dc = DonchianChannel::new(5);
    let highs = [110, 108, 112, 106, 114, 104, 116];
    let lows = [90, 88, 92, 86, 94, 84, 96];
    for (i, (&h, &l)) in highs.iter().zip(lows.iter()).enumerate() {
        let r = dc.update_bar(&bar(
            i as i64,
            Decimal::from(h),
            Decimal::from(l),
            Decimal::from((h + l) / 2),
        ));
        if r.is_ready() {
            assert!(
                dc.upper >= dc.lower,
                "Upper ({}) must be >= Lower ({})",
                dc.upper,
                dc.lower
            );
        }
    }
}

#[test]
fn donchian_middle_is_average_of_upper_lower() {
    let mut dc = DonchianChannel::new(5);
    for i in 0..7 {
        let h = Decimal::from(100 + i);
        let l = Decimal::from(90 + i);
        let r = dc.update_bar(&bar(i as i64, h, l, (h + l) / dec!(2)));
        if r.is_ready() {
            let expected_mid = (dc.upper + dc.lower) / dec!(2);
            assert_approx(
                dc.middle,
                expected_mid,
                dec!(0.001),
                "Middle should be (upper+lower)/2",
            );
        }
    }
}

#[test]
fn donchian_upper_is_period_high() {
    let mut dc = DonchianChannel::new(3);
    // Bars: H = 100, 105, 110
    dc.update_bar(&bar(0, dec!(100), dec!(90), dec!(95)));
    dc.update_bar(&bar(1, dec!(105), dec!(92), dec!(98)));
    dc.update_bar(&bar(2, dec!(110), dec!(88), dec!(100)));
    assert!(dc.is_ready());
    assert_eq!(dc.upper, dec!(110));
    assert_eq!(dc.lower, dec!(88));
}

#[test]
fn donchian_reset_clears_state() {
    let mut dc = DonchianChannel::new(5);
    for i in 0..7 {
        dc.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(dc.is_ready());

    dc.reset();

    assert!(!dc.is_ready());
    assert_eq!(dc.samples(), 0);
}

// ─── Keltner Channel Tests ────────────────────────────────────────────────────

#[test]
fn keltner_not_ready_before_warmup() {
    // warm_up_period = ema.warm_up_period + 1 = period + 1 (for EMA) + 1 (ATR needs period+1)
    let mut kc = KeltnerChannel::new(5, dec!(2));
    assert!(!kc.is_ready());
    // Feed only a few bars
    for i in 0..5 {
        kc.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    // EMA needs 5, ATR needs 5+1=6 → kc not ready after 5
    assert!(!kc.is_ready());
}

#[test]
fn keltner_upper_above_lower() {
    let mut kc = KeltnerChannel::new(5, dec!(2));
    for i in 0..15 {
        let h = Decimal::from(100 + (i % 5));
        let l = Decimal::from(90 - (i % 3));
        let c = (h + l) / dec!(2);
        let r = kc.update_bar(&bar(i as i64, h, l, c));
        if r.is_ready() {
            assert!(
                kc.upper > kc.lower,
                "Upper ({}) must be > Lower ({})",
                kc.upper,
                kc.lower
            );
        }
    }
}

#[test]
fn keltner_middle_is_ema() {
    let mut kc = KeltnerChannel::new(5, dec!(2));
    // When ready, middle should equal EMA of close
    let mut ema_standalone = Ema::new(5);
    let closes = [
        100, 101, 99, 102, 98, 103, 97, 104, 96, 105, 100, 101, 99, 102, 98,
    ];
    for (i, &c) in closes.iter().enumerate() {
        let cv = Decimal::from(c);
        let h = cv + dec!(5);
        let l = cv - dec!(5);
        kc.update_bar(&bar(i as i64, h, l, cv));
        ema_standalone.update_price(ts(i as i64), cv);
    }
    if kc.is_ready() && ema_standalone.is_ready() {
        assert_approx(
            kc.middle,
            ema_standalone.current().value,
            dec!(0.01),
            "Keltner middle should equal EMA of close",
        );
    }
}

#[test]
fn keltner_produces_finite_values() {
    let mut kc = KeltnerChannel::new(5, dec!(2));
    for i in 0..20 {
        let c = Decimal::from(100 + i);
        let r = kc.update_bar(&bar(i as i64, c + dec!(3), c - dec!(3), c));
        if r.is_ready() {
            assert!(kc.upper > dec!(0), "Keltner upper must be positive");
            assert!(kc.lower > dec!(0), "Keltner lower must be positive");
            assert!(kc.middle > dec!(0), "Keltner middle must be positive");
        }
    }
}

// ─── VWAP Tests ───────────────────────────────────────────────────────────────

#[test]
fn vwap_ready_after_first_bar() {
    let mut vwap = Vwap::new();
    assert!(!vwap.is_ready());
    let r = vwap.update_bar(&bar(0, dec!(105), dec!(95), dec!(100)));
    assert!(r.is_ready());
}

#[test]
fn vwap_single_bar_equals_typical_price() {
    let mut vwap = Vwap::new();
    // Typical = (H + L + C) / 3 = (110 + 90 + 100) / 3 = 100
    let r = vwap.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(110),
        dec!(90),
        dec!(100),
        dec!(1000),
    ));
    assert!(r.is_ready());
    assert_approx(
        r.value,
        dec!(100),
        dec!(0.001),
        "VWAP of single bar should equal typical price",
    );
}

#[test]
fn vwap_weighted_toward_high_volume_bars() {
    let mut vwap = Vwap::new();
    // Bar 1: typical = 100, volume = 100
    // Bar 2: typical = 200, volume = 900
    // VWAP = (100*100 + 200*900) / 1000 = 190
    vwap.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(102),
        dec!(98),
        dec!(100),
        dec!(100),
    ));
    let r = vwap.update_bar(&make_bar(
        1,
        dec!(200),
        dec!(202),
        dec!(198),
        dec!(200),
        dec!(900),
    ));
    assert!(r.is_ready());
    assert_approx(r.value, dec!(190), dec!(0.1), "VWAP should be 190");
}

#[test]
fn vwap_reset_session_clears_cumulative() {
    // reset_session clears cumulative PV/volume but not `samples`.
    // The `is_ready` check uses samples > 0, so it remains true.
    // However, after reset_session the VWAP recalculates from the next bar only.
    let mut vwap = Vwap::new();
    vwap.update_bar(&bar(0, dec!(110), dec!(90), dec!(100)));
    vwap.update_bar(&bar(1, dec!(110), dec!(90), dec!(100)));
    assert!(vwap.is_ready());

    vwap.reset_session();

    // After reset_session, next bar computes fresh VWAP
    // Bar: O=200 H=202 L=198 C=200 → typical = (202+198+200)/3 = 200
    let r = vwap.update_bar(&make_bar(
        2,
        dec!(200),
        dec!(202),
        dec!(198),
        dec!(200),
        dec!(1000),
    ));
    assert!(r.is_ready());
    assert_approx(
        r.value,
        dec!(200),
        dec!(0.001),
        "After reset_session, VWAP = first bar typical",
    );
}

#[test]
fn vwap_warm_up_period_is_one() {
    let vwap = Vwap::new();
    assert_eq!(vwap.warm_up_period(), 1);
}

// ─── OBV Tests ────────────────────────────────────────────────────────────────

#[test]
fn obv_ready_after_first_bar() {
    let mut obv = Obv::new();
    assert!(!obv.is_ready());
    let r = obv.update_bar(&bar(0, dec!(110), dec!(90), dec!(100)));
    assert!(r.is_ready());
}

#[test]
fn obv_first_bar_is_zero() {
    let mut obv = Obv::new();
    // First bar has no previous close, OBV = 0
    let r = obv.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(110),
        dec!(90),
        dec!(100),
        dec!(1000),
    ));
    assert_eq!(r.value, dec!(0));
}

#[test]
fn obv_adds_volume_on_up_close() {
    let mut obv = Obv::new();
    obv.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(110),
        dec!(90),
        dec!(100),
        dec!(1000),
    ));
    // Close higher than previous (100 → 110)
    let r = obv.update_bar(&make_bar(
        1,
        dec!(110),
        dec!(115),
        dec!(105),
        dec!(110),
        dec!(2000),
    ));
    assert!(r.is_ready());
    assert_eq!(r.value, dec!(2000), "OBV should add volume on up close");
}

#[test]
fn obv_subtracts_volume_on_down_close() {
    let mut obv = Obv::new();
    obv.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(110),
        dec!(90),
        dec!(100),
        dec!(1000),
    ));
    // Close lower (100 → 90)
    let r = obv.update_bar(&make_bar(
        1,
        dec!(90),
        dec!(95),
        dec!(85),
        dec!(90),
        dec!(1500),
    ));
    assert!(r.is_ready());
    assert_eq!(
        r.value,
        dec!(-1500),
        "OBV should subtract volume on down close"
    );
}

#[test]
fn obv_cumulative_across_bars() {
    let mut obv = Obv::new();
    // Bar 0: OBV = 0 (base)
    // Bar 1: up, +2000 → OBV = 2000
    // Bar 2: down, -1000 → OBV = 1000
    // Bar 3: up, +3000 → OBV = 4000
    obv.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(105),
        dec!(95),
        dec!(100),
        dec!(1000),
    ));
    obv.update_bar(&make_bar(
        1,
        dec!(102),
        dec!(108),
        dec!(98),
        dec!(102),
        dec!(2000),
    ));
    obv.update_bar(&make_bar(
        2,
        dec!(100),
        dec!(106),
        dec!(96),
        dec!(100),
        dec!(1000),
    ));
    let r = obv.update_bar(&make_bar(
        3,
        dec!(105),
        dec!(110),
        dec!(100),
        dec!(105),
        dec!(3000),
    ));
    assert_eq!(r.value, dec!(4000));
}

#[test]
fn obv_unchanged_on_equal_close() {
    let mut obv = Obv::new();
    obv.update_bar(&make_bar(
        0,
        dec!(100),
        dec!(105),
        dec!(95),
        dec!(100),
        dec!(1000),
    ));
    // Same close = no change to OBV
    let r = obv.update_bar(&make_bar(
        1,
        dec!(100),
        dec!(105),
        dec!(95),
        dec!(100),
        dec!(500),
    ));
    assert_eq!(r.value, dec!(0), "OBV should not change on equal close");
}

// ─── MFI Tests ────────────────────────────────────────────────────────────────

#[test]
fn mfi_not_ready_before_period_bars() {
    // is_ready = window.is_full() (window capacity = period).
    // The window fills after `period` bars, making MFI ready.
    let mut mfi = MoneyFlowIndex::new(5);
    assert!(!mfi.is_ready());
    for i in 0..4 {
        mfi.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
        assert!(
            !mfi.is_ready(),
            "MFI should not be ready after {} bars",
            i + 1
        );
    }
    mfi.update_bar(&bar(4, dec!(110), dec!(90), dec!(100)));
    assert!(mfi.is_ready(), "MFI should be ready after {} bars", 5);
}

#[test]
fn mfi_warm_up_period_equals_period_plus_one() {
    let mfi = MoneyFlowIndex::new(14);
    assert_eq!(mfi.warm_up_period(), 15);
}

#[test]
fn mfi_all_positive_flow_gives_100() {
    // Continuously rising typical price → all money flow is positive → MFI = 100
    let mut mfi = MoneyFlowIndex::new(5);
    for i in 0..8 {
        let c = Decimal::from(100 + i * 2);
        mfi.update_bar(&make_bar(
            i as i64,
            c,
            c + dec!(1),
            c - dec!(1),
            c,
            dec!(1000),
        ));
    }
    if mfi.is_ready() {
        assert_approx(
            mfi.current().value,
            dec!(100),
            dec!(0.001),
            "MFI should be 100 when all money flow is positive",
        );
    }
}

#[test]
fn mfi_all_negative_flow_gives_zero() {
    // Continuously falling → all negative → MFI = 0
    let mut mfi = MoneyFlowIndex::new(5);
    for i in 0..8 {
        let c = Decimal::from(100 - i * 2);
        mfi.update_bar(&make_bar(
            i as i64,
            c,
            c + dec!(1),
            c - dec!(1),
            c,
            dec!(1000),
        ));
    }
    if mfi.is_ready() {
        assert_approx(
            mfi.current().value,
            dec!(0),
            dec!(0.001),
            "MFI should be 0 when all money flow is negative",
        );
    }
}

#[test]
fn mfi_bounded_0_to_100() {
    let mut mfi = MoneyFlowIndex::new(5);
    let closes = [100, 102, 98, 103, 97, 105, 95, 108, 92, 110, 100];
    for (i, &c) in closes.iter().enumerate() {
        let cv = Decimal::from(c);
        let r = mfi.update_bar(&make_bar(
            i as i64,
            cv,
            cv + dec!(2),
            cv - dec!(2),
            cv,
            dec!(1000),
        ));
        if r.is_ready() {
            assert!(
                r.value >= dec!(0) && r.value <= dec!(100),
                "MFI out of bounds: {}",
                r.value
            );
        }
    }
}

#[test]
fn mfi_reset_clears_state() {
    let mut mfi = MoneyFlowIndex::new(5);
    for i in 0..8 {
        mfi.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(mfi.is_ready());

    mfi.reset();

    assert!(!mfi.is_ready());
    assert_eq!(mfi.samples(), 0);
}

// ─── Aroon Tests ──────────────────────────────────────────────────────────────

#[test]
fn aroon_not_ready_before_period_plus_one() {
    let mut aroon = Aroon::new(5);
    assert!(!aroon.is_ready());
    for i in 0..5 {
        aroon.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
        assert!(!aroon.is_ready());
    }
    aroon.update_bar(&bar(5, dec!(110), dec!(90), dec!(100)));
    assert!(aroon.is_ready());
}

#[test]
fn aroon_warm_up_period_equals_period_plus_one() {
    let aroon = Aroon::new(25);
    assert_eq!(aroon.warm_up_period(), 26);
}

#[test]
fn aroon_up_100_when_high_is_most_recent() {
    let mut aroon = Aroon::new(5);
    // Rising then flat: last bar has the highest high
    let bars_data = [
        (100, 90),
        (101, 91),
        (102, 92),
        (103, 93),
        (104, 94),
        (110, 96),
    ];
    for (i, &(h, l)) in bars_data.iter().enumerate() {
        aroon.update_bar(&bar(
            i as i64,
            Decimal::from(h),
            Decimal::from(l),
            Decimal::from((h + l) / 2),
        ));
    }
    assert!(aroon.is_ready());
    // Most recent bar has the highest high → Aroon Up = 100
    assert_eq!(
        aroon.up,
        dec!(100),
        "Aroon Up should be 100 when high is at position 0 (newest)"
    );
}

#[test]
fn aroon_down_100_when_low_is_most_recent() {
    let mut aroon = Aroon::new(5);
    // Descending lows: last bar has the lowest low
    let bars_data = [(100, 90), (99, 89), (98, 88), (97, 87), (96, 86), (95, 80)];
    for (i, &(h, l)) in bars_data.iter().enumerate() {
        aroon.update_bar(&bar(
            i as i64,
            Decimal::from(h),
            Decimal::from(l),
            Decimal::from((h + l) / 2),
        ));
    }
    assert!(aroon.is_ready());
    assert_eq!(
        aroon.down,
        dec!(100),
        "Aroon Down should be 100 when low is most recent"
    );
}

#[test]
fn aroon_up_and_down_bounded_0_to_100() {
    let mut aroon = Aroon::new(5);
    for i in 0..15 {
        let h = Decimal::from(100 + (i % 7));
        let l = Decimal::from(90 - (i % 5));
        let r = aroon.update_bar(&bar(i as i64, h, l, (h + l) / dec!(2)));
        if r.is_ready() {
            assert!(
                aroon.up >= dec!(0) && aroon.up <= dec!(100),
                "Aroon Up out of bounds: {}",
                aroon.up
            );
            assert!(
                aroon.down >= dec!(0) && aroon.down <= dec!(100),
                "Aroon Down out of bounds: {}",
                aroon.down
            );
        }
    }
}

#[test]
fn aroon_oscillator_is_up_minus_down() {
    let mut aroon = Aroon::new(5);
    for i in 0..10 {
        let h = Decimal::from(100 + i);
        let l = Decimal::from(90 + i);
        let r = aroon.update_bar(&bar(i as i64, h, l, (h + l) / dec!(2)));
        if r.is_ready() {
            // current().value is up - down (oscillator)
            let expected = aroon.up - aroon.down;
            assert_approx(
                r.value,
                expected,
                dec!(0.001),
                "Aroon oscillator should equal Up - Down",
            );
        }
    }
}

#[test]
fn aroon_reset_clears_state() {
    let mut aroon = Aroon::new(5);
    for i in 0..8 {
        aroon.update_bar(&bar(i, dec!(110), dec!(90), dec!(100)));
    }
    assert!(aroon.is_ready());

    aroon.reset();

    assert!(!aroon.is_ready());
    assert_eq!(aroon.samples(), 0);
}

// ─── Cross-indicator integration checks ───────────────────────────────────────

#[test]
fn bollinger_upper_above_lower_always() {
    let mut bb = BollingerBands::standard(20);
    for i in 0..30 {
        let c = Decimal::from(100 + (i % 10) * 3 - 5);
        let r = bb.update_price(ts(i as i64), c);
        if r.is_ready() {
            assert!(
                bb.upper >= bb.lower,
                "BB upper ({}) must be >= lower ({})",
                bb.upper,
                bb.lower
            );
        }
    }
}

#[test]
fn bollinger_middle_equals_sma() {
    let mut bb = BollingerBands::standard(5);
    let mut sma = Sma::new(5);
    let prices = [100, 102, 98, 104, 96, 108, 94, 106, 100, 103];
    for (i, &p) in prices.iter().enumerate() {
        let pv = Decimal::from(p);
        let bb_r = bb.update_price(ts(i as i64), pv);
        let sma_r = sma.update_price(ts(i as i64), pv);
        if bb_r.is_ready() && sma_r.is_ready() {
            assert_approx(
                bb.middle,
                sma_r.value,
                dec!(0.001),
                "Bollinger middle should equal SMA",
            );
        }
    }
}

#[test]
fn bollinger_wider_for_volatile_prices() {
    // Calm series then volatile series — width should be larger for volatile
    let mut bb_calm = BollingerBands::standard(5);
    let mut bb_volatile = BollingerBands::standard(5);

    for i in 0..5 {
        let cv = Decimal::from(100);
        bb_calm.update_price(ts(i), cv);
    }

    for i in 0..5 {
        let cv = Decimal::from(100 + (i % 2) * 20 - 10); // swings ±10
        bb_volatile.update_price(ts(i), cv);
    }

    // bandwidth = (upper - lower) / middle
    if bb_calm.is_ready() && bb_volatile.is_ready() {
        assert!(
            bb_volatile.bandwidth > bb_calm.bandwidth,
            "Volatile BB width ({}) should exceed calm ({})",
            bb_volatile.bandwidth,
            bb_calm.bandwidth
        );
    }
}

#[test]
fn rsi_values_bounded_0_to_100() {
    let mut rsi = Rsi::new(14);
    let prices = [
        44, 44, 44, 47, 43, 49, 41, 51, 39, 53, 37, 55, 35, 57, 33, 59, 31, 61,
    ];
    for (i, &p) in prices.iter().enumerate() {
        let r = rsi.update_price(ts(i as i64), Decimal::from(p));
        if r.is_ready() {
            assert!(
                r.value >= dec!(0) && r.value <= dec!(100),
                "RSI out of bounds: {}",
                r.value
            );
        }
    }
}

#[test]
fn macd_histogram_equals_macd_minus_signal() {
    use lean_indicators::Macd;
    let mut macd = Macd::new(12, 26, 9);
    let prices = [
        44, 44, 44, 44, 44, 44, 44, 44, 44, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57,
        58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70,
    ];
    for (i, &p) in prices.iter().enumerate() {
        let r = macd.update_price(ts(i as i64), Decimal::from(p));
        if r.is_ready() {
            let expected_hist = macd.macd_line - macd.signal_line;
            assert_approx(
                macd.histogram,
                expected_hist,
                dec!(0.0000001),
                "MACD histogram should equal MACD - signal",
            );
        }
    }
}
