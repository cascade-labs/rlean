use std::collections::HashMap;
use rust_decimal::Decimal;
use lean_core::Symbol;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// Maximum Sharpe Ratio portfolio construction using a diagonal covariance assumption.
///
/// With a diagonal covariance matrix, the maximum Sharpe ratio (tangency) portfolio
/// has analytical weights proportional to:
///
///   w_i ∝ (μ_i - rf) / σ_i²
///
/// which is the same formula as mean-variance with λ=1, but normalized differently.
/// Weights are normalized so their absolute values sum to 1.
///
/// Mirrors C# MaximumSharpeRatioPortfolioOptimizer (used inside
/// BlackLittermanOptimizationPortfolioConstructionModel).
pub struct MaximumSharpeRatioPortfolioConstructionModel {
    pub risk_free_rate: f64,
    pub annual_vol: f64,
}

impl MaximumSharpeRatioPortfolioConstructionModel {
    pub fn new() -> Self {
        Self {
            risk_free_rate: 0.0,
            annual_vol: 0.20,
        }
    }

    pub fn with_params(risk_free_rate: f64, annual_vol: f64) -> Self {
        Self {
            risk_free_rate,
            annual_vol,
        }
    }

    fn expected_return(insight: &InsightForPcm) -> f64 {
        let default_mag = 0.01_f64;
        let magnitude = insight
            .magnitude
            .map(|m| m.abs().to_string().parse::<f64>().unwrap_or(default_mag))
            .unwrap_or(default_mag);

        match insight.direction {
            InsightDirection::Up => magnitude,
            InsightDirection::Down => -magnitude,
            InsightDirection::Flat => 0.0,
        }
    }
}

impl Default for MaximumSharpeRatioPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for MaximumSharpeRatioPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        if insights.is_empty() {
            return vec![];
        }

        let sigma_sq = self.annual_vol * self.annual_vol;
        let rf = self.risk_free_rate;

        // Tangency portfolio weights: w_i ∝ (μ_i - rf) / σ_i²
        let raw_weights: Vec<f64> = insights
            .iter()
            .map(|i| {
                let mu = Self::expected_return(i);
                (mu - rf) / sigma_sq
            })
            .collect();

        // Normalize by abs sum so portfolio is fully invested
        let abs_sum: f64 = raw_weights.iter().map(|w| w.abs()).sum();
        let normalized: Vec<f64> = if abs_sum < 1e-12 {
            // Degenerate case: fall back to equal weights for active insights
            let n_active = insights
                .iter()
                .filter(|i| i.direction != InsightDirection::Flat)
                .count();
            insights
                .iter()
                .map(|i| {
                    if i.direction == InsightDirection::Flat || n_active == 0 {
                        0.0
                    } else {
                        i.direction.as_i32() as f64 / n_active as f64
                    }
                })
                .collect()
        } else {
            raw_weights.iter().map(|w| w / abs_sum).collect()
        };

        insights
            .iter()
            .zip(normalized.iter())
            .map(|(insight, &weight_f64)| {
                let pct = Decimal::try_from(weight_f64).unwrap_or(Decimal::ZERO);
                let ticker = insight.symbol.value.clone();
                let price = prices.get(&ticker).copied().unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "MaximumSharpeRatioPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
