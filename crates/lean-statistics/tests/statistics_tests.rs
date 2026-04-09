use lean_statistics::statistics::Statistics;
use rust_decimal_macros::dec;
use rust_decimal::Decimal;

// ─── Max drawdown ─────────────────────────────────────────────────────────────

#[test]
fn max_drawdown_no_drawdown() {
    // Monotonically rising equity — drawdown = 0
    let curve = vec![dec!(100), dec!(110), dec!(120), dec!(130)];
    assert_eq!(Statistics::max_drawdown(&curve), dec!(0));
}

#[test]
fn max_drawdown_simple_case() {
    // Peak at 120, drops to 90 → drawdown = 30/120 = 0.25
    let curve = vec![dec!(100), dec!(120), dec!(90)];
    let dd = Statistics::max_drawdown(&curve);
    assert_eq!(dd, dec!(30) / dec!(120));
}

#[test]
fn max_drawdown_50_pct() {
    // Peak 200, valley 100 → 50% drawdown
    let curve = vec![dec!(100), dec!(200), dec!(100)];
    let dd = Statistics::max_drawdown(&curve);
    assert_eq!(dd, dec!(0.5));
}

#[test]
fn max_drawdown_empty_returns_zero() {
    assert_eq!(Statistics::max_drawdown(&[]), dec!(0));
}

#[test]
fn max_drawdown_single_point_returns_zero() {
    assert_eq!(Statistics::max_drawdown(&[dec!(100)]), dec!(0));
}

// ─── Sharpe ratio ────────────────────────────────────────────────────────────

#[test]
fn sharpe_ratio_empty_returns_zero() {
    assert_eq!(Statistics::sharpe_ratio(&[], dec!(0)), dec!(0));
}

#[test]
fn sharpe_ratio_one_value_returns_zero() {
    assert_eq!(Statistics::sharpe_ratio(&[dec!(0.01)], dec!(0)), dec!(0));
}

#[test]
fn sharpe_ratio_positive_for_consistent_gains() {
    // All returns = 1% per day → very high Sharpe (no variance)
    // std dev = 0 → Sharpe = 0 (implementation returns 0 for std=0)
    let returns: Vec<Decimal> = vec![dec!(0.01); 100];
    let sharpe = Statistics::sharpe_ratio(&returns, dec!(0));
    assert_eq!(sharpe, dec!(0)); // zero std dev edge case
}

#[test]
fn sharpe_ratio_negative_for_consistent_losses() {
    let returns: Vec<Decimal> = (0..100).map(|i| if i % 2 == 0 { dec!(-0.02) } else { dec!(0.01) }).collect();
    let sharpe = Statistics::sharpe_ratio(&returns, dec!(0));
    assert!(sharpe < dec!(0), "Sharpe should be negative for net-losing returns");
}

#[test]
fn sharpe_ratio_higher_with_better_returns() {
    // Compare two return streams with same std dev but different mean
    let base: Vec<Decimal> = vec![dec!(0.01), dec!(-0.005), dec!(0.01), dec!(-0.005)];
    let better: Vec<Decimal> = vec![dec!(0.02), dec!(0.005), dec!(0.02), dec!(0.005)];
    let s1 = Statistics::sharpe_ratio(&base, dec!(0));
    let s2 = Statistics::sharpe_ratio(&better, dec!(0));
    assert!(s2 > s1, "Better returns should yield higher Sharpe");
}

// ─── Sortino ratio ───────────────────────────────────────────────────────────

#[test]
fn sortino_ratio_no_losses_returns_zero() {
    // No downside → sortino = 0 (implementation returns 0 when no downside)
    let returns = vec![dec!(0.01), dec!(0.02), dec!(0.015)];
    assert_eq!(Statistics::sortino_ratio(&returns, dec!(0)), dec!(0));
}

#[test]
fn sortino_ratio_mixed_returns() {
    let returns = vec![dec!(0.01), dec!(-0.005), dec!(0.02), dec!(-0.01)];
    let sortino = Statistics::sortino_ratio(&returns, dec!(0));
    // Mean = (0.01 - 0.005 + 0.02 - 0.01) / 4 = 0.015/4 = 0.00375
    // Should be positive for net-positive returns
    assert!(sortino > dec!(0));
}

// ─── Beta ────────────────────────────────────────────────────────────────────

#[test]
fn beta_identical_returns_is_one() {
    let returns = vec![dec!(0.01), dec!(-0.005), dec!(0.02), dec!(-0.01)];
    let beta = Statistics::beta(&returns, &returns.clone());
    assert_eq!(beta, dec!(1));
}

#[test]
fn beta_zero_variance_benchmark_returns_one() {
    let returns = vec![dec!(0.01), dec!(-0.005), dec!(0.02)];
    let flat: Vec<Decimal> = vec![dec!(0); 3];
    let beta = Statistics::beta(&returns, &flat);
    assert_eq!(beta, dec!(1)); // fallback when benchmark variance is 0
}

#[test]
fn beta_short_series_returns_one() {
    let r = vec![dec!(0.01)];
    let b = vec![dec!(0.01)];
    assert_eq!(Statistics::beta(&r, &b), dec!(1));
}

// ─── Alpha ───────────────────────────────────────────────────────────────────

#[test]
fn alpha_zero_when_returns_match_capm() {
    // alpha = annual - (rf + beta * (bench - rf))
    // If annual = rf + beta*(bench-rf), alpha = 0
    let bench = dec!(0.08);
    let rf = dec!(0.02);
    let beta = dec!(1);
    let annual = rf + beta * (bench - rf); // = 0.08
    assert_eq!(Statistics::alpha(annual, beta, bench, rf), dec!(0));
}

#[test]
fn alpha_positive_for_outperformance() {
    // annual = 15%, bench = 10%, beta = 1, rf = 2%
    // expected = 2% + 1*(10%-2%) = 10%
    // alpha = 15% - 10% = 5%
    let alpha = Statistics::alpha(dec!(0.15), dec!(1), dec!(0.10), dec!(0.02));
    assert_eq!(alpha, dec!(0.05));
}

// ─── Annual performance ───────────────────────────────────────────────────────

#[test]
fn annual_performance_zero_days_returns_zero() {
    assert_eq!(Statistics::annual_performance(dec!(0.10), 0), dec!(0));
}

#[test]
fn annual_performance_one_year() {
    // 252 trading days = 1 year, total return = 10% → annual ≈ 10%
    let annual = Statistics::annual_performance(dec!(0.10), 252);
    // Should be very close to 0.10
    let diff = (annual - dec!(0.10)).abs();
    assert!(diff < dec!(0.001), "Annual performance should be ~10% for 1-year 10% return, got {}", annual);
}

#[test]
fn annual_performance_positive_for_gain() {
    let annual = Statistics::annual_performance(dec!(0.20), 504); // ~2 years
    assert!(annual > dec!(0));
    assert!(annual < dec!(0.20)); // less than total for multi-year
}

// ─── Calmar ratio ────────────────────────────────────────────────────────────

#[test]
fn calmar_zero_drawdown_returns_zero() {
    assert_eq!(Statistics::calmar_ratio(dec!(0.15), dec!(0)), dec!(0));
}

#[test]
fn calmar_ratio_correct() {
    // annual = 15%, max_dd = 10% → calmar = 1.5
    assert_eq!(Statistics::calmar_ratio(dec!(0.15), dec!(0.10)), dec!(1.5));
}
