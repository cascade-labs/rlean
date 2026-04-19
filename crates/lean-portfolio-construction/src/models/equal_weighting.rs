use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// Gives equal weighting to all active non-Flat insights.
/// Mirrors C# EqualWeightingPortfolioConstructionModel.
///
/// For each active insight:
///   - Up    -> long  at +1/N of portfolio value
///   - Down  -> short at -1/N of portfolio value
///   - Flat  -> zero target (liquidate)
///
/// where N is the number of Up/Down insights.
pub struct EqualWeightingPortfolioConstructionModel;

impl EqualWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EqualWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for EqualWeightingPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        // Count non-Flat insights (mirrors C# count logic)
        let active_count = insights
            .iter()
            .filter(|i| i.direction != InsightDirection::Flat)
            .count();

        let weight = if active_count == 0 {
            Decimal::ZERO
        } else {
            Decimal::ONE / Decimal::from(active_count)
        };

        insights
            .iter()
            .map(|insight| {
                let direction_sign = Decimal::from(insight.direction.as_i32());
                // Flat insights get 0 weight per C# logic
                let pct = if insight.direction == InsightDirection::Flat {
                    Decimal::ZERO
                } else {
                    direction_sign * weight
                };

                let ticker = insight.symbol.value.clone();
                let price = prices.get(&ticker).copied().unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "EqualWeightingPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
