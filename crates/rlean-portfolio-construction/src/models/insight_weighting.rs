use rlean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm, RebalancePolicy,
};
use crate::{PortfolioBias, PortfolioTarget};

/// Generates percent targets from `Insight.weight`, matching C# LEAN.
///
/// Insights without a weight are ignored. If the gross weight is greater than
/// one, targets are scaled proportionally so gross exposure is one.
pub struct InsightWeightingPortfolioConstructionModel {
    portfolio_bias: PortfolioBias,
    rebalance_policy: RebalancePolicy,
}

impl InsightWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self::with_bias_and_rebalance_policy(PortfolioBias::LongShort, RebalancePolicy::daily())
    }

    pub fn with_bias_and_rebalance_policy(
        portfolio_bias: PortfolioBias,
        rebalance_policy: RebalancePolicy,
    ) -> Self {
        Self {
            portfolio_bias,
            rebalance_policy,
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
        prices: &HashMap<u64, Decimal>,
    ) -> Vec<PortfolioTarget> {
        let eligible: Vec<&InsightForPcm> = insights
            .iter()
            .filter(|insight| insight.weight.is_some())
            .collect();
        let weight_sum: Decimal = eligible
            .iter()
            .filter(|insight| self.respects_bias(insight))
            .map(|insight| insight.weight.unwrap_or_default().abs())
            .sum();
        let weight_factor = if weight_sum > Decimal::ONE {
            Decimal::ONE / weight_sum
        } else {
            Decimal::ONE
        };

        eligible
            .into_iter()
            .map(|insight| {
                let direction = if self.respects_bias(insight) {
                    Decimal::from(insight.direction.as_i32())
                } else {
                    Decimal::ZERO
                };
                let percent = direction * insight.weight.unwrap_or_default().abs() * weight_factor;
                let price = prices
                    .get(&insight.symbol.id.sid)
                    .copied()
                    .unwrap_or_default();
                PortfolioTarget::percent(insight.symbol.clone(), percent, portfolio_value, price)
            })
            .collect()
    }

    fn rebalance_policy(&self) -> RebalancePolicy {
        self.rebalance_policy.clone()
    }

    fn name(&self) -> &str {
        "InsightWeightingPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
