use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// Weights insights by their confidence value, normalized so the sum <= 1.
/// Mirrors C# InsightWeightingPortfolioConstructionModel.
///
/// - Uses `confidence` as the weight (abs value).
/// - If total weight > 1, scales all down proportionally.
/// - Insights with no confidence are excluded (weight = 0).
/// - Falls back to equal weighting when all confidences are None/zero.
pub struct InsightWeightingPortfolioConstructionModel;

impl InsightWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InsightWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for InsightWeightingPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        // Get the absolute confidence value for each insight (0 if None).
        let get_value = |insight: &InsightForPcm| -> Decimal {
            insight.confidence.map(|c| c.abs()).unwrap_or(Decimal::ZERO)
        };

        // Sum weights for all non-Flat insights to detect normalization need.
        // Mirrors C# weightSums / weightFactor logic.
        let weight_sum: Decimal = insights
            .iter()
            .filter(|i| i.direction != InsightDirection::Flat)
            .map(get_value)
            .fold(Decimal::ZERO, |acc, v| acc + v);

        let weight_factor = if weight_sum > Decimal::ONE {
            Decimal::ONE / weight_sum
        } else {
            Decimal::ONE
        };

        // If all confidences are zero/None, fall back to equal weighting.
        let active_non_flat = insights
            .iter()
            .filter(|i| i.direction != InsightDirection::Flat)
            .count();

        let use_equal = weight_sum == Decimal::ZERO && active_non_flat > 0;
        let equal_weight = if use_equal && active_non_flat > 0 {
            Decimal::ONE / Decimal::from(active_non_flat)
        } else {
            Decimal::ZERO
        };

        insights
            .iter()
            .map(|insight| {
                let direction_sign = Decimal::from(insight.direction.as_i32());

                let pct = if insight.direction == InsightDirection::Flat {
                    Decimal::ZERO
                } else if use_equal {
                    direction_sign * equal_weight
                } else {
                    direction_sign * get_value(insight) * weight_factor
                };

                let ticker = insight.symbol.value.clone();
                let price = prices.get(&ticker).copied().unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "InsightWeightingPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
