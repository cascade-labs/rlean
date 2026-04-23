/// Unit tests for risk management models.
///
/// Based on the LEAN C# test suite behaviour for:
///   - MaximumDrawdownPercentPortfolio
///   - MaximumUnrealizedProfitPercentPerSecurity
///   - MaximumSectorExposureRiskManagementModel (basic exposure cap)
use lean_core::{Market, Symbol};
use lean_risk::{
    max_drawdown_portfolio::MaximumDrawdownPercentPortfolio,
    max_unrealized_profit::MaximumUnrealizedProfitPercentPerSecurity,
    risk_management::{HoldingSnapshot, PortfolioTarget, RiskContext, RiskManagementModel},
    sector_exposure::MaximumSectorExposureRiskManagementModel,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ─── helpers ──────────────────────────────────────────────────────────────────

fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn aapl() -> Symbol {
    Symbol::create_equity("AAPL", &Market::usa())
}

fn make_target(symbol: Symbol, quantity: Decimal) -> PortfolioTarget {
    PortfolioTarget::new(symbol, quantity)
}

fn holding(symbol: Symbol, qty: Decimal, avg: Decimal, last: Decimal) -> HoldingSnapshot {
    let unrealized_pnl = (last - avg) * qty;
    HoldingSnapshot {
        symbol,
        quantity: qty,
        average_price: avg,
        last_price: last,
        unrealized_pnl,
    }
}

// ─── MaximumDrawdownPercentPortfolio ─────────────────────────────────────────

#[test]
fn test_drawdown_portfolio_no_trigger_below_threshold() {
    let mut model = MaximumDrawdownPercentPortfolio::new(dec!(0.05), false);

    // Start at 100 000
    let ctx = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![],
    };
    let targets = vec![make_target(spy(), dec!(100))];

    // First call — initialises high at 100 000
    let result = model.manage_risk_with_context(&targets, &ctx);
    // At exactly the high, drawdown == 0 — no liquidation
    assert!(
        result.is_empty(),
        "Should not liquidate when at portfolio high: {result:?}"
    );
}

#[test]
fn test_drawdown_portfolio_no_trigger_small_drawdown() {
    let mut model = MaximumDrawdownPercentPortfolio::new(dec!(0.05), false);

    // Initialise at 100 000
    let ctx_init = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![],
    };
    let targets = vec![make_target(spy(), dec!(100))];
    model.manage_risk_with_context(&targets, &ctx_init);

    // Drop 3% — below 5% threshold
    let ctx_drop = RiskContext {
        total_portfolio_value: dec!(97_000),
        holdings: vec![],
    };
    let result = model.manage_risk_with_context(&targets, &ctx_drop);
    assert!(
        result.is_empty(),
        "Should not liquidate on 3% drawdown with 5% threshold"
    );
}

#[test]
fn test_drawdown_portfolio_triggers_at_threshold() {
    let mut model = MaximumDrawdownPercentPortfolio::new(dec!(0.05), false);

    let targets = vec![make_target(spy(), dec!(100))];

    // Initialise at 100 000
    let ctx_init = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![],
    };
    model.manage_risk_with_context(&targets, &ctx_init);

    // Drop 6% — exceeds 5% threshold
    let ctx_drop = RiskContext {
        total_portfolio_value: dec!(94_000),
        holdings: vec![],
    };
    let result = model.manage_risk_with_context(&targets, &ctx_drop);
    assert_eq!(result.len(), 1, "Should emit one liquidation target");
    assert_eq!(result[0].quantity, Decimal::ZERO, "Liquidation qty must be 0");
    assert_eq!(result[0].symbol.value, "SPY");
}

#[test]
fn test_drawdown_portfolio_resets_after_trigger() {
    let mut model = MaximumDrawdownPercentPortfolio::new(dec!(0.05), false);
    let targets = vec![make_target(spy(), dec!(100))];

    let ctx_init = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![],
    };
    model.manage_risk_with_context(&targets, &ctx_init);

    // Trigger drawdown
    let ctx_drop = RiskContext {
        total_portfolio_value: dec!(90_000),
        holdings: vec![],
    };
    model.manage_risk_with_context(&targets, &ctx_drop);

    // After reset, the next call should re-initialise at the new value (90 000),
    // and a further 3% drop should NOT trigger.
    let ctx_small_drop = RiskContext {
        total_portfolio_value: dec!(87_300), // 3% below 90 000
        holdings: vec![],
    };
    let result = model.manage_risk_with_context(&targets, &ctx_small_drop);
    assert!(
        result.is_empty(),
        "After reset, small drop should not trigger again: {result:?}"
    );
}

#[test]
fn test_drawdown_portfolio_trailing_updates_high() {
    let mut model = MaximumDrawdownPercentPortfolio::new(dec!(0.05), true);
    let targets = vec![make_target(spy(), dec!(100))];

    // Start at 100k
    let ctx1 = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![],
    };
    model.manage_risk_with_context(&targets, &ctx1);

    // Rise to 110k — new high-water mark, returns empty
    let ctx2 = RiskContext {
        total_portfolio_value: dec!(110_000),
        holdings: vec![],
    };
    let result = model.manage_risk_with_context(&targets, &ctx2);
    assert!(result.is_empty(), "At new high should not liquidate");

    // Now drop 6% from 110k → 103 400 — exceeds threshold
    let ctx3 = RiskContext {
        total_portfolio_value: dec!(103_400),
        holdings: vec![],
    };
    let result = model.manage_risk_with_context(&targets, &ctx3);
    assert_eq!(result.len(), 1, "Should liquidate when trailing drawdown exceeded");
    assert_eq!(result[0].quantity, Decimal::ZERO);
}

// ─── MaximumUnrealizedProfitPercentPerSecurity ───────────────────────────────

#[test]
fn test_unrealized_profit_no_trigger_below_threshold() {
    let mut model = MaximumUnrealizedProfitPercentPerSecurity::new(dec!(0.05));

    // Holding SPY, bought at 400, now at 410 → 2.5% profit (< 5%)
    let ctx = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![holding(spy(), dec!(10), dec!(400), dec!(410))],
    };
    let result = model.manage_risk_with_context(&[], &ctx);
    assert!(
        result.is_empty(),
        "2.5% profit should not trigger 5% threshold"
    );
}

#[test]
fn test_unrealized_profit_triggers_above_threshold() {
    let mut model = MaximumUnrealizedProfitPercentPerSecurity::new(dec!(0.05));

    // Holding SPY, bought at 400, now at 430 → 7.5% profit (> 5%)
    let ctx = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![holding(spy(), dec!(10), dec!(400), dec!(430))],
    };
    let result = model.manage_risk_with_context(&[], &ctx);
    assert_eq!(result.len(), 1, "7.5% profit should trigger 5% threshold");
    assert_eq!(result[0].quantity, Decimal::ZERO);
    assert_eq!(result[0].symbol.value, "SPY");
}

#[test]
fn test_unrealized_profit_skips_uninvested() {
    let mut model = MaximumUnrealizedProfitPercentPerSecurity::new(dec!(0.05));

    // qty == 0 means not invested
    let ctx = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![holding(spy(), dec!(0), dec!(400), dec!(430))],
    };
    let result = model.manage_risk_with_context(&[], &ctx);
    assert!(result.is_empty(), "Uninvested holding should be skipped");
}

#[test]
fn test_unrealized_profit_multiple_holdings_mixed() {
    let mut model = MaximumUnrealizedProfitPercentPerSecurity::new(dec!(0.05));

    let ctx = RiskContext {
        total_portfolio_value: dec!(200_000),
        holdings: vec![
            // SPY: 7.5% profit → should trigger
            holding(spy(), dec!(10), dec!(400), dec!(430)),
            // AAPL: 2.5% profit → should NOT trigger
            holding(aapl(), dec!(5), dec!(150), dec!(153_75) / dec!(100)),
        ],
    };
    let result = model.manage_risk_with_context(&[], &ctx);
    assert_eq!(result.len(), 1, "Only SPY should be liquidated");
    assert_eq!(result[0].symbol.value, "SPY");
}

#[test]
fn test_unrealized_profit_short_position() {
    let mut model = MaximumUnrealizedProfitPercentPerSecurity::new(dec!(0.05));

    // Short SPY: sold at 400, now at 370 → 7.5% profit (short profit)
    let ctx = RiskContext {
        total_portfolio_value: dec!(100_000),
        holdings: vec![holding(spy(), dec!(-10), dec!(400), dec!(370))],
    };
    let result = model.manage_risk_with_context(&[], &ctx);
    assert_eq!(result.len(), 1, "Short position with 7.5% profit should trigger");
    assert_eq!(result[0].quantity, Decimal::ZERO);
}

// ─── MaximumSectorExposureRiskManagementModel ─────────────────────────────────

#[test]
fn test_sector_exposure_pass_through_when_no_violation() {
    let mut model = MaximumSectorExposureRiskManagementModel::new(dec!(0.20));

    // The stub passes targets through — verify it doesn't panic and returns targets.
    let targets = vec![
        make_target(spy(), dec!(100)),
        make_target(aapl(), dec!(50)),
    ];
    let result = model.manage_risk(&targets);
    // Current stub impl passes targets through unchanged.
    assert_eq!(result.len(), 2);
}

#[test]
fn test_sector_exposure_model_construction() {
    // Verify the model can be constructed with various weights and sector mappings.
    let mut model = MaximumSectorExposureRiskManagementModel::new(dec!(0.30));
    model.set_sector(1, "Technology".to_string());
    model.set_sector(2, "Healthcare".to_string());
    assert_eq!(model.max_sector_weight, dec!(0.30));
}

#[test]
fn test_sector_exposure_zero_targets() {
    let mut model = MaximumSectorExposureRiskManagementModel::new(dec!(0.20));
    let result = model.manage_risk(&[]);
    assert!(result.is_empty(), "Empty targets should produce empty result");
}
