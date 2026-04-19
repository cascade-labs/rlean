use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// Groups insights by their symbol's market, applies equal weight within each
/// market group, and equal weight across market groups.
///
/// This is a Cascade Labs extension — not present in C# LEAN by this name.
///
/// Example: 3 markets (US, CRYPTO, FOREX), each with N_k non-Flat insights:
///   weight_per_insight_in_group_k = 1/num_markets * 1/N_k
pub struct SectorWeightingPortfolioConstructionModel;

impl SectorWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SectorWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for SectorWeightingPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        // Group non-Flat insights by market string.
        let mut market_groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, insight) in insights.iter().enumerate() {
            if insight.direction != InsightDirection::Flat {
                let market_key = insight.symbol.id.market.as_str().to_string();
                market_groups.entry(market_key).or_default().push(idx);
            }
        }

        let num_markets = market_groups.len();
        let market_weight = if num_markets == 0 {
            Decimal::ZERO
        } else {
            Decimal::ONE / Decimal::from(num_markets)
        };

        // Compute per-insight weight: market_weight / group_size
        let mut per_insight_weight: HashMap<usize, Decimal> = HashMap::new();
        for indices in market_groups.values() {
            let group_size = indices.len();
            let w = if group_size == 0 {
                Decimal::ZERO
            } else {
                market_weight / Decimal::from(group_size)
            };
            for &idx in indices {
                per_insight_weight.insert(idx, w);
            }
        }

        insights
            .iter()
            .enumerate()
            .map(|(idx, insight)| {
                let direction_sign = Decimal::from(insight.direction.as_i32());
                let w = per_insight_weight
                    .get(&idx)
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                let pct = direction_sign * w;

                let ticker = insight.symbol.value.clone();
                let price = prices.get(&ticker).copied().unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "SectorWeightingPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
