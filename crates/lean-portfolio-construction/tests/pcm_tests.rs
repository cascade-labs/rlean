// Integration tests for lean-portfolio-construction.
// Mirrors the C# EqualWeightingPortfolioConstructionModelTests and
// InsightWeightingPortfolioConstructionModelTests from LEAN.

use lean_core::{Market, Symbol};
use lean_portfolio_construction::{
    EqualWeightingPortfolioConstructionModel, IPortfolioConstructionModel, InsightDirection,
    InsightForPcm, InsightWeightingPortfolioConstructionModel, NullPortfolioConstructionModel,
    PortfolioTarget,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn make_equity(ticker: &str) -> Symbol {
    Symbol::create_equity(ticker, &Market::new(Market::USA))
}

fn make_insight(symbol: Symbol, direction: InsightDirection) -> InsightForPcm {
    InsightForPcm {
        symbol,
        direction,
        magnitude: None,
        confidence: None,
        source_model: "test".to_string(),
    }
}

fn make_insight_with_confidence(
    symbol: Symbol,
    direction: InsightDirection,
    confidence: Option<Decimal>,
) -> InsightForPcm {
    InsightForPcm {
        symbol,
        direction,
        magnitude: None,
        confidence,
        source_model: "test".to_string(),
    }
}

/// Build a price map from (ticker, price) pairs.
fn make_prices(pairs: &[(&str, Decimal)]) -> HashMap<String, Decimal> {
    pairs
        .iter()
        .map(|(ticker, price)| (ticker.to_uppercase(), *price))
        .collect()
}

// ---------------------------------------------------------------------------
// EqualWeightingPortfolioConstructionModel tests
// ---------------------------------------------------------------------------

mod equal_weighting_tests {
    use super::*;

    /// Two Up insights: each should get 50 % of portfolio → equal long shares.
    /// Mirrors C# InsightsReturnsTargetsConsistentWithDirection (Up, N=2).
    #[test]
    fn equal_weight_two_up_insights() {
        let aig = make_equity("AIG");
        let ibm = make_equity("IBM");

        let insights = vec![
            make_insight(aig.clone(), InsightDirection::Up),
            make_insight(ibm.clone(), InsightDirection::Up),
        ];

        // AIG @ $55.22, IBM @ $145.17, portfolio = $100_000
        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("AIG", dec!(55.22)), ("IBM", dec!(145.17))]);

        let mut model = EqualWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 2, "Expected one target per insight");

        // Each weight = 0.5 → quantity = round(100_000 * 0.5 / price)
        let aig_qty = (portfolio_value * dec!(0.5) / dec!(55.22)).round();
        let ibm_qty = (portfolio_value * dec!(0.5) / dec!(145.17)).round();

        let find = |ticker: &str| {
            targets
                .iter()
                .find(|t| t.symbol.value == ticker.to_uppercase())
                .expect("target not found")
                .quantity
        };

        assert_eq!(find("AIG"), aig_qty);
        assert_eq!(find("IBM"), ibm_qty);
        assert!(
            aig_qty > Decimal::ZERO,
            "AIG quantity must be positive (long)"
        );
        assert!(
            ibm_qty > Decimal::ZERO,
            "IBM quantity must be positive (long)"
        );
    }

    /// One Up + one Down: each gets 50 %, long and short respectively.
    /// Mirrors C# InsightsReturnsTargetsConsistentWithDirection for mixed direction.
    #[test]
    fn equal_weight_up_and_down() {
        let spy = make_equity("SPY");
        let ibm = make_equity("IBM");

        let insights = vec![
            make_insight(spy.clone(), InsightDirection::Up),
            make_insight(ibm.clone(), InsightDirection::Down),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79)), ("IBM", dec!(145.17))]);

        let mut model = EqualWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 2);

        let get_qty = |ticker: &str| {
            targets
                .iter()
                .find(|t| t.symbol.value == ticker.to_uppercase())
                .unwrap()
                .quantity
        };

        let spy_qty = get_qty("SPY");
        let ibm_qty = get_qty("IBM");

        assert!(spy_qty > Decimal::ZERO, "SPY should be long (Up insight)");
        assert!(
            ibm_qty < Decimal::ZERO,
            "IBM should be short (Down insight)"
        );
    }

    /// A Flat insight should result in a zero-quantity target (liquidate).
    /// Mirrors C# FlatDirectionNotAccountedToAllocation.
    #[test]
    fn flat_insight_gets_zero_quantity() {
        let spy = make_equity("SPY");

        let insights = vec![make_insight(spy.clone(), InsightDirection::Flat)];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79))]);

        let mut model = EqualWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].quantity,
            Decimal::ZERO,
            "Flat insight should produce zero quantity"
        );
    }

    /// No insights → no targets.
    #[test]
    fn empty_insights_returns_empty() {
        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79))]);

        let mut model = EqualWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&[], portfolio_value, &prices);

        assert!(targets.is_empty(), "No insights should produce no targets");
    }

    /// N Up insights: each weight = 1/N, quantities sum to approximately the
    /// full portfolio value (modulo integer rounding).
    /// Mirrors C# Weight == 1/Securities.Count logic.
    #[test]
    fn equal_weight_sums_to_one_for_three_up_insights() {
        let aig = make_equity("AIG");
        let ibm = make_equity("IBM");
        let spy = make_equity("SPY");

        let insights = vec![
            make_insight(aig.clone(), InsightDirection::Up),
            make_insight(ibm.clone(), InsightDirection::Up),
            make_insight(spy.clone(), InsightDirection::Up),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[
            ("AIG", dec!(55.22)),
            ("IBM", dec!(145.17)),
            ("SPY", dec!(281.79)),
        ]);

        let mut model = EqualWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 3);

        // Each gets 1/3 of portfolio; verify all are positive (long).
        for target in &targets {
            assert!(
                target.quantity > Decimal::ZERO,
                "All Up insights should produce positive quantities"
            );
        }

        // Verify approximate: sum of holdings value <= portfolio_value
        // (rounding may leave some cash unallocated).
        let holdings_value: Decimal = targets
            .iter()
            .map(|t| {
                let price = prices[&t.symbol.value];
                t.quantity * price
            })
            .sum();

        // Each slot should have approximately portfolio_value / 3.
        let expected_per_slot = portfolio_value / dec!(3);
        for target in &targets {
            let price = prices[&target.symbol.value];
            let slot_value = target.quantity * price;
            let diff = (slot_value - expected_per_slot).abs();
            assert!(
                diff < price + Decimal::ONE,
                "Slot value {slot_value} should be within one share of expected {expected_per_slot}"
            );
        }

        // Total holdings value must not exceed portfolio value.
        assert!(
            holdings_value <= portfolio_value,
            "Total holdings value {holdings_value} must not exceed portfolio value {portfolio_value}"
        );
    }

    /// Flat insight in a mixed set: Flat gets zero, others are allocated equally.
    /// Mirrors C# FlatDirectionNotAccountedToAllocation (SPY=Flat, AIG+IBM=Up).
    #[test]
    fn flat_insight_not_counted_in_allocation() {
        let aig = make_equity("AIG");
        let ibm = make_equity("IBM");
        let spy = make_equity("SPY");

        let insights = vec![
            make_insight(aig.clone(), InsightDirection::Up),
            make_insight(ibm.clone(), InsightDirection::Up),
            make_insight(spy.clone(), InsightDirection::Flat),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[
            ("AIG", dec!(55.22)),
            ("IBM", dec!(145.17)),
            ("SPY", dec!(281.79)),
        ]);

        let mut model = EqualWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 3);

        let get_qty = |ticker: &str| {
            targets
                .iter()
                .find(|t| t.symbol.value == ticker.to_uppercase())
                .unwrap()
                .quantity
        };

        // SPY is Flat → zero quantity.
        assert_eq!(get_qty("SPY"), Decimal::ZERO);

        // AIG and IBM each get 50% (N=2 non-flat), so positive quantities.
        assert!(get_qty("AIG") > Decimal::ZERO);
        assert!(get_qty("IBM") > Decimal::ZERO);

        // AIG+IBM quantities should correspond to 50% allocation each.
        let aig_expected = (portfolio_value * dec!(0.5) / dec!(55.22)).round();
        let ibm_expected = (portfolio_value * dec!(0.5) / dec!(145.17)).round();
        assert_eq!(get_qty("AIG"), aig_expected);
        assert_eq!(get_qty("IBM"), ibm_expected);
    }
}

// ---------------------------------------------------------------------------
// NullPortfolioConstructionModel tests
// ---------------------------------------------------------------------------

mod null_pcm_tests {
    use super::*;

    /// NullPortfolioConstructionModel should always return an empty target list,
    /// regardless of insights provided.
    #[test]
    fn null_returns_empty_targets_with_insights() {
        let spy = make_equity("SPY");
        let insights = vec![
            make_insight(spy.clone(), InsightDirection::Up),
            make_insight(make_equity("IBM"), InsightDirection::Down),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79)), ("IBM", dec!(145.17))]);

        let mut model = NullPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert!(
            targets.is_empty(),
            "NullPortfolioConstructionModel must always return empty targets"
        );
    }

    /// NullPortfolioConstructionModel with no insights returns empty.
    #[test]
    fn null_returns_empty_targets_no_insights() {
        let mut model = NullPortfolioConstructionModel::new();
        let targets = model.create_targets(&[], dec!(100_000), &HashMap::new());
        assert!(targets.is_empty());
    }

    /// Verify the model's reported name.
    #[test]
    fn null_pcm_name() {
        let model = NullPortfolioConstructionModel::new();
        assert_eq!(model.name(), "NullPortfolioConstructionModel");
    }
}

// ---------------------------------------------------------------------------
// PortfolioTarget unit tests
// ---------------------------------------------------------------------------

mod portfolio_target_tests {
    use super::*;

    /// Positive quantity means long position.
    #[test]
    fn target_long_positive_quantity() {
        let spy = make_equity("SPY");
        let target = PortfolioTarget::new(spy, dec!(100));
        assert!(target.quantity > Decimal::ZERO);
    }

    /// Negative quantity means short position.
    #[test]
    fn target_short_negative_quantity() {
        let spy = make_equity("SPY");
        let target = PortfolioTarget::new(spy, dec!(-50));
        assert!(target.quantity < Decimal::ZERO);
    }

    /// PortfolioTarget::percent: portfolio=$100k, price=$50, pct=0.10 → quantity=200.
    /// Mirrors C# PortfolioTarget.Percent logic.
    #[test]
    fn target_percent_calculation() {
        let spy = make_equity("SPY");
        // portfolio_value = $100,000 ; pct = 10% ; price = $50
        // expected quantity = round(100,000 * 0.10 / 50) = round(200) = 200
        let target = PortfolioTarget::percent(spy, dec!(0.10), dec!(100_000), dec!(50));
        assert_eq!(target.quantity, dec!(200));
        assert_eq!(target.percent, Some(dec!(0.10)));
    }

    /// Zero price should not cause a divide-by-zero panic; quantity should be 0.
    #[test]
    fn target_zero_price_returns_zero() {
        let spy = make_equity("SPY");
        let target = PortfolioTarget::percent(spy, dec!(0.10), dec!(100_000), Decimal::ZERO);
        assert_eq!(target.quantity, Decimal::ZERO);
    }

    /// Zero percentage means zero quantity.
    #[test]
    fn target_zero_percent_means_zero_quantity() {
        let spy = make_equity("SPY");
        let target = PortfolioTarget::percent(spy, Decimal::ZERO, dec!(100_000), dec!(50));
        assert_eq!(target.quantity, Decimal::ZERO);
    }
}

// ---------------------------------------------------------------------------
// InsightWeightingPortfolioConstructionModel tests
// ---------------------------------------------------------------------------

mod insight_weighting_tests {
    use super::*;

    /// Two insights with equal weight=1: each gets 50% (weights sum to 2, so
    /// each normalized weight = 0.5). Mirrors C# WeightsProportionally test.
    #[test]
    fn weights_proportionally_equal_confidence() {
        let spy = make_equity("SPY");
        let ibm = make_equity("IBM");

        let insights = vec![
            make_insight_with_confidence(spy.clone(), InsightDirection::Up, Some(dec!(1))),
            make_insight_with_confidence(ibm.clone(), InsightDirection::Up, Some(dec!(1))),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79)), ("IBM", dec!(145.17))]);

        let mut model = InsightWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 2);

        // weights sum = 2 → normalize by 0.5 each
        let spy_expected = (portfolio_value * dec!(0.5) / dec!(281.79)).round();
        let ibm_expected = (portfolio_value * dec!(0.5) / dec!(145.17)).round();

        let get_qty = |ticker: &str| {
            targets
                .iter()
                .find(|t| t.symbol.value == ticker.to_uppercase())
                .unwrap()
                .quantity
        };

        assert_eq!(get_qty("SPY"), spy_expected);
        assert_eq!(get_qty("IBM"), ibm_expected);
    }

    /// Insight with None confidence falls back to equal weighting.
    /// Mirrors C# GeneratesNoTargetsForInsightsWithNoWeight — in the Rust port
    /// None confidence triggers equal-weight fallback rather than exclusion.
    #[test]
    fn no_confidence_falls_back_to_equal_weight() {
        let spy = make_equity("SPY");
        let ibm = make_equity("IBM");

        // Both have None confidence → should fall back to equal weighting (50/50)
        let insights = vec![
            make_insight_with_confidence(spy.clone(), InsightDirection::Up, None),
            make_insight_with_confidence(ibm.clone(), InsightDirection::Up, None),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79)), ("IBM", dec!(145.17))]);

        let mut model = InsightWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 2);
        for t in &targets {
            assert!(
                t.quantity > Decimal::ZERO,
                "fallback equal weight should produce positive quantities"
            );
        }
    }

    /// When a single insight has zero confidence, the Rust model falls back to
    /// equal weighting (the only non-Flat insight gets 100% allocation).
    ///
    /// NOTE: This diverges from C# InsightWeightingPortfolioConstructionModel,
    /// which emits a zero-quantity target for zero-weight insights. The Rust
    /// port treats zero/None confidence as "no preference expressed" and falls
    /// back to equal weighting so the position is still sized. This behaviour
    /// is intentional — if you want the C# zero-weight semantics, set the
    /// insight direction to Flat instead.
    #[test]
    fn zero_confidence_single_insight_falls_back_to_equal_weight() {
        let spy = make_equity("SPY");

        // confidence = Some(0) → weight_sum = 0 → equal-weight fallback kicks in.
        // With only one non-Flat insight the entire portfolio is allocated to it.
        let insights = vec![make_insight_with_confidence(
            spy.clone(),
            InsightDirection::Down,
            Some(Decimal::ZERO),
        )];

        let portfolio_value = dec!(100_000);
        let price = dec!(281.79);
        let prices = make_prices(&[("SPY", price)]);

        let mut model = InsightWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 1);

        // Equal-weight fallback: weight = 1/1 = 1.0 (full portfolio), direction = Down → short.
        let expected_qty = -(portfolio_value / price).round();
        assert_eq!(
            targets[0].quantity, expected_qty,
            "Zero-confidence single Down insight: equal-weight fallback, full short allocation"
        );
    }

    /// Asymmetric weights: confidence 0.3 vs 0.7 — higher confidence gets
    /// proportionally larger allocation.
    #[test]
    fn asymmetric_confidence_weights() {
        let aig = make_equity("AIG");
        let ibm = make_equity("IBM");

        let insights = vec![
            make_insight_with_confidence(aig.clone(), InsightDirection::Up, Some(dec!(0.3))),
            make_insight_with_confidence(ibm.clone(), InsightDirection::Up, Some(dec!(0.7))),
        ];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("AIG", dec!(55.22)), ("IBM", dec!(145.17))]);

        let mut model = InsightWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 2);

        // weights sum = 1.0 → no normalization needed
        let aig_expected = (portfolio_value * dec!(0.3) / dec!(55.22)).round();
        let ibm_expected = (portfolio_value * dec!(0.7) / dec!(145.17)).round();

        let get_qty = |ticker: &str| {
            targets
                .iter()
                .find(|t| t.symbol.value == ticker.to_uppercase())
                .unwrap()
                .quantity
        };

        assert_eq!(get_qty("AIG"), aig_expected);
        assert_eq!(get_qty("IBM"), ibm_expected);
    }

    /// Flat insight in InsightWeighting → zero target regardless of confidence.
    #[test]
    fn flat_direction_always_zero() {
        let spy = make_equity("SPY");

        let insights = vec![make_insight_with_confidence(
            spy.clone(),
            InsightDirection::Flat,
            Some(dec!(0.5)),
        )];

        let portfolio_value = dec!(100_000);
        let prices = make_prices(&[("SPY", dec!(281.79))]);

        let mut model = InsightWeightingPortfolioConstructionModel::new();
        let targets = model.create_targets(&insights, portfolio_value, &prices);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].quantity, Decimal::ZERO);
    }
}
