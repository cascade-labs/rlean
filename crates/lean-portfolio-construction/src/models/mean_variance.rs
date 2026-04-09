use std::collections::HashMap;
use rust_decimal::Decimal;
use lean_core::Symbol;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// Simplified Markowitz mean-variance optimization using a diagonal covariance
/// assumption (no cross-asset correlations).
///
/// Mirrors C# MeanVarianceOptimizationPortfolioConstructionModel in spirit,
/// but uses an analytical closed-form solution rather than a quadratic solver:
///
///   w_i = (μ_i - rf) / (σ_i² * λ)
///
/// where:
///   μ_i  = expected return from insight direction + magnitude
///            Up   → +magnitude (or +0.01 if None)
///            Down → -magnitude (or -0.01 if None)
///            Flat → 0.0
///   σ_i  = assumed annual volatility = 0.20 (20%)
///   λ    = risk aversion coefficient (default 1.0)
///   rf   = risk-free rate (default 0.0)
///
/// Weights are then normalized so they sum to 1.0 in absolute value.
pub struct MeanVariancePortfolioConstructionModel {
    pub risk_aversion: f64,
    pub risk_free_rate: f64,
    pub annual_vol: f64,
}

impl MeanVariancePortfolioConstructionModel {
    pub fn new() -> Self {
        Self {
            risk_aversion: 1.0,
            risk_free_rate: 0.0,
            annual_vol: 0.20,
        }
    }

    pub fn with_params(risk_aversion: f64, risk_free_rate: f64, annual_vol: f64) -> Self {
        Self {
            risk_aversion,
            risk_free_rate,
            annual_vol,
        }
    }

    fn expected_return(insight: &InsightForPcm) -> f64 {
        let default_magnitude = 0.01_f64;
        let magnitude = insight
            .magnitude
            .map(|m| m.abs().to_string().parse::<f64>().unwrap_or(default_magnitude))
            .unwrap_or(default_magnitude);

        match insight.direction {
            InsightDirection::Up => magnitude,
            InsightDirection::Down => -magnitude,
            InsightDirection::Flat => 0.0,
        }
    }
}

impl Default for MeanVariancePortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for MeanVariancePortfolioConstructionModel {
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
        let lambda = self.risk_aversion;
        let rf = self.risk_free_rate;

        // Compute raw (unnormalized) weights w_i = (μ_i - rf) / (σ_i² * λ)
        let raw_weights: Vec<f64> = insights
            .iter()
            .map(|i| {
                let mu = Self::expected_return(i);
                (mu - rf) / (sigma_sq * lambda)
            })
            .collect();

        // Normalize so the absolute weight sum = 1 (prevents over-leveraging)
        let abs_sum: f64 = raw_weights.iter().map(|w| w.abs()).sum();
        let normalized: Vec<f64> = if abs_sum < 1e-12 {
            // All weights ~zero, return equal weights for Up/Down insights
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
        "MeanVariancePortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
