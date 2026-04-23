use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// Weights each position by its insight's `confidence` value, normalized so the sum
/// of all confidence weights is at most 1.0.
///
/// Mirrors C# `ConfidenceWeightedPortfolioConstructionModel`, which is a thin subclass
/// of `InsightWeightingPortfolioConstructionModel` that:
///   - Skips insights with `confidence = None` entirely (no target generated).
///   - Uses `confidence` as the raw weight (rather than a separate magnitude field).
///   - Normalises weights if their sum exceeds 1.
///   - Falls back to equal weighting when all non-Flat insights have `confidence = 0`.
///
/// Direction sign (Up = +1, Down = −1) is multiplied into the weight to produce the
/// signed target percentage.
pub struct ConfidenceWeightingPortfolioConstructionModel;

impl ConfidenceWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ConfidenceWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for ConfidenceWeightingPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        // Key difference vs InsightWeighting: skip insights with no confidence value.
        let eligible: Vec<&InsightForPcm> = insights
            .iter()
            .filter(|i| i.confidence.is_some())
            .collect();

        if eligible.is_empty() {
            return Vec::new();
        }

        // Compute the total weight of all non-Flat, eligible insights.
        let weight_sum: Decimal = eligible
            .iter()
            .filter(|i| i.direction != InsightDirection::Flat)
            .map(|i| i.confidence.unwrap_or(Decimal::ZERO).abs())
            .fold(Decimal::ZERO, |acc, v| acc + v);

        let weight_factor = if weight_sum > Decimal::ONE {
            Decimal::ONE / weight_sum
        } else {
            Decimal::ONE
        };

        // Note: unlike InsightWeightingPortfolioConstructionModel, ConfidenceWeighted does
        // NOT fall back to equal weighting when all confidences are zero.  If weight_sum == 0,
        // every non-Flat insight simply gets pct = direction * 0 = 0 (matching C# behaviour).

        eligible
            .iter()
            .map(|insight| {
                let direction_sign = Decimal::from(insight.direction.as_i32());
                let confidence = insight.confidence.unwrap_or(Decimal::ZERO).abs();

                let pct = if insight.direction == InsightDirection::Flat {
                    Decimal::ZERO
                } else {
                    direction_sign * confidence * weight_factor
                };

                let ticker = insight.symbol.value.clone();
                let price = prices.get(&ticker).copied().unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "ConfidenceWeightingPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, Symbol};
    use rust_decimal_macros::dec;

    fn make_symbol(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    fn make_prices(pairs: &[(&str, Decimal)]) -> HashMap<String, Decimal> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }

    #[test]
    fn high_confidence_gets_higher_weight_than_low_confidence() {
        let mut pcm = ConfidenceWeightingPortfolioConstructionModel::new();
        let portfolio_value = dec!(100_000);

        let spy = make_symbol("SPY");
        let ibm = make_symbol("IBM");
        let prices = make_prices(&[("SPY", dec!(100)), ("IBM", dec!(100))]);

        let insights = vec![
            InsightForPcm {
                symbol: spy.clone(),
                direction: InsightDirection::Up,
                magnitude: None,
                confidence: Some(dec!(0.8)), // high confidence
                source_model: "test".to_string(),
            },
            InsightForPcm {
                symbol: ibm.clone(),
                direction: InsightDirection::Up,
                magnitude: None,
                confidence: Some(dec!(0.2)), // low confidence
                source_model: "test".to_string(),
            },
        ];

        let targets = pcm.create_targets(&insights, portfolio_value, &prices);
        assert_eq!(targets.len(), 2);

        let spy_target = targets.iter().find(|t| t.symbol.value == "SPY").unwrap();
        let ibm_target = targets.iter().find(|t| t.symbol.value == "IBM").unwrap();

        // SPY confidence is 4x IBM, so SPY quantity should be 4x IBM quantity.
        assert!(
            spy_target.quantity > ibm_target.quantity,
            "SPY qty {} should exceed IBM qty {}",
            spy_target.quantity,
            ibm_target.quantity
        );
        assert_eq!(spy_target.quantity, ibm_target.quantity * dec!(4));
    }

    #[test]
    fn insights_without_confidence_are_excluded() {
        let mut pcm = ConfidenceWeightingPortfolioConstructionModel::new();
        let portfolio_value = dec!(100_000);

        let spy = make_symbol("SPY");
        let prices = make_prices(&[("SPY", dec!(100))]);

        let insights = vec![InsightForPcm {
            symbol: spy.clone(),
            direction: InsightDirection::Down,
            magnitude: None,
            confidence: None, // no confidence → excluded
            source_model: "test".to_string(),
        }];

        let targets = pcm.create_targets(&insights, portfolio_value, &prices);
        assert!(
            targets.is_empty(),
            "Expected no targets for insight with confidence=None"
        );
    }

    #[test]
    fn zero_confidence_produces_zero_target() {
        // Mirrors C# test "GeneratesZeroTargetForZeroInsightConfidence".
        // confidence=0 is included (not filtered out like None), but produces weight=0,
        // so pct=0 and qty=0. No equal-weight fallback in ConfidenceWeighted.
        let mut pcm = ConfidenceWeightingPortfolioConstructionModel::new();
        let portfolio_value = dec!(100_000);

        let spy = make_symbol("SPY");
        let prices = make_prices(&[("SPY", dec!(100))]);

        let insights = vec![InsightForPcm {
            symbol: spy.clone(),
            direction: InsightDirection::Down,
            magnitude: None,
            confidence: Some(dec!(0)),
            source_model: "test".to_string(),
        }];

        let targets = pcm.create_targets(&insights, portfolio_value, &prices);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].quantity, dec!(0));
    }

    #[test]
    fn weights_normalize_when_sum_exceeds_one() {
        let mut pcm = ConfidenceWeightingPortfolioConstructionModel::new();
        let portfolio_value = dec!(100_000);

        let spy = make_symbol("SPY");
        let ibm = make_symbol("IBM");
        let prices = make_prices(&[("SPY", dec!(100)), ("IBM", dec!(100))]);

        // Two insights each with confidence=1 → sum=2, normalize to 0.5 each.
        let insights = vec![
            InsightForPcm {
                symbol: spy.clone(),
                direction: InsightDirection::Down,
                magnitude: None,
                confidence: Some(dec!(1)),
                source_model: "test".to_string(),
            },
            InsightForPcm {
                symbol: ibm.clone(),
                direction: InsightDirection::Down,
                magnitude: None,
                confidence: Some(dec!(1)),
                source_model: "test".to_string(),
            },
        ];

        let targets = pcm.create_targets(&insights, portfolio_value, &prices);
        assert_eq!(targets.len(), 2);

        // Each gets pct = -0.5, qty = floor(100000 * 0.5 / 100) = 500 (short)
        for t in &targets {
            assert_eq!(t.quantity, dec!(-500));
        }
    }
}
