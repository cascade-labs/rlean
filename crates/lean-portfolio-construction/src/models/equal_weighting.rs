use lean_core::{Symbol, TimeSpan};
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;
use crate::PortfolioBias;

/// Gives equal weighting to all active non-Flat insights.
/// Mirrors C# EqualWeightingPortfolioConstructionModel.
///
/// For each active insight:
///   - Up    -> long  at +1/N of portfolio value
///   - Down  -> short at -1/N of portfolio value
///   - Flat  -> zero target (liquidate)
///
/// where N is the number of Up/Down insights.
pub struct EqualWeightingPortfolioConstructionModel {
    portfolio_bias: PortfolioBias,
    max_weight: Option<Decimal>,
    rebalance_period: Option<TimeSpan>,
}

impl EqualWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self {
            portfolio_bias: PortfolioBias::LongShort,
            max_weight: None,
            rebalance_period: Some(TimeSpan::ONE_DAY),
        }
    }

    pub fn with_bias(portfolio_bias: PortfolioBias) -> Self {
        Self {
            portfolio_bias,
            max_weight: None,
            rebalance_period: Some(TimeSpan::ONE_DAY),
        }
    }

    pub fn with_bias_and_max_weight(
        portfolio_bias: PortfolioBias,
        max_weight: Option<Decimal>,
    ) -> Self {
        Self {
            portfolio_bias,
            max_weight,
            rebalance_period: Some(TimeSpan::ONE_DAY),
        }
    }

    pub fn with_bias_max_weight_and_rebalance(
        portfolio_bias: PortfolioBias,
        max_weight: Option<Decimal>,
        rebalance_period: Option<TimeSpan>,
    ) -> Self {
        Self {
            portfolio_bias,
            max_weight,
            rebalance_period,
        }
    }

    fn respects_bias(&self, insight: &InsightForPcm) -> bool {
        match self.portfolio_bias {
            PortfolioBias::LongShort => true,
            PortfolioBias::Long => insight.direction == InsightDirection::Up,
            PortfolioBias::Short => insight.direction == InsightDirection::Down,
        }
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
        prices: &HashMap<u64, Decimal>,
    ) -> Vec<PortfolioTarget> {
        // Count non-Flat insights (mirrors C# count logic)
        let active_count = insights
            .iter()
            .filter(|i| i.direction != InsightDirection::Flat && self.respects_bias(i))
            .count();

        let weight = if active_count == 0 {
            Decimal::ZERO
        } else {
            let equal_weight = Decimal::ONE / Decimal::from(active_count);
            self.max_weight
                .map(|max_weight| equal_weight.min(max_weight))
                .unwrap_or(equal_weight)
        };

        insights
            .iter()
            .map(|insight| {
                let direction_sign = if self.respects_bias(insight) {
                    Decimal::from(insight.direction.as_i32())
                } else {
                    Decimal::ZERO
                };
                // Flat insights get 0 weight per C# logic
                let pct = if insight.direction == InsightDirection::Flat {
                    Decimal::ZERO
                } else {
                    direction_sign * weight
                };

                let price = prices
                    .get(&insight.symbol.id.sid)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "EqualWeightingPortfolioConstructionModel"
    }

    fn rebalance_period(&self) -> Option<TimeSpan> {
        self.rebalance_period
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
