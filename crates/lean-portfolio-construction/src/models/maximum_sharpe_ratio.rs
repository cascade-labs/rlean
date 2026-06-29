use lean_core::{DateTime, Symbol};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;

use crate::portfolio_construction_model::{IPortfolioConstructionModel, InsightForPcm};
use crate::portfolio_target::PortfolioTarget;

use super::black_litterman::PortfolioBias;
use super::matrix::{column_means, covariance_matrix, mat_scale, mean_variance_weights};
use super::returns_symbol_data::{form_returns_matrix, ReturnsSymbolData};

/// Maximum Sharpe Ratio portfolio construction, mirroring C#
/// `MaximumSharpeRatioPortfolioOptimizer` (the default optimizer inside
/// Black-Litterman).
///
/// Like LEAN, this maintains a rolling history of realised returns per asset
/// (via [`ReturnsSymbolData`], which wraps a rate-of-change indicator) and uses
/// the resulting annualised sample covariance, rather than assuming a constant
/// per-asset volatility.
///
/// The tangency (maximum-Sharpe) portfolio has the closed-form solution
/// `w ∝ Σ⁻¹ (μ − rf·1)` (see [`mean_variance_weights`]); this is the analytical
/// solution that LEAN's optimizer computes via the Charnes-Cooper substitution
/// and a quadratic solver. Returns are annualised with the standard 252 factor.
pub struct MaximumSharpeRatioPortfolioConstructionModel {
    /// Rate-of-change lookback in bars (default 1).
    lookback: usize,
    /// Rolling window length in bars (default 63 ≈ one quarter).
    period: usize,
    /// Risk-free rate used in the Sharpe numerator (default 0.0).
    risk_free_rate: f64,
    /// Long/Short/LongShort bias applied to the optimizer output.
    portfolio_bias: PortfolioBias,
    /// Per-symbol rolling returns history (ROC indicator + rolling window).
    asset_data: HashMap<String, ReturnsSymbolData>,
}

impl MaximumSharpeRatioPortfolioConstructionModel {
    /// Create with default parameters (matches C# defaults).
    pub fn new() -> Self {
        Self::with_params(1, 63, 0.0, PortfolioBias::LongShort)
    }

    pub fn with_params(
        lookback: usize,
        period: usize,
        risk_free_rate: f64,
        portfolio_bias: PortfolioBias,
    ) -> Self {
        Self {
            lookback: lookback.max(1),
            period: period.max(2),
            risk_free_rate,
            portfolio_bias,
            asset_data: HashMap::new(),
        }
    }

    /// Update rolling returns from the current price map.
    fn update_prices(&mut self, prices: &HashMap<String, Decimal>) {
        let now = DateTime::now();
        for (ticker, price_dec) in prices {
            let price = price_dec.to_f64().unwrap_or(0.0);
            if price <= 0.0 {
                continue;
            }
            self.asset_data
                .entry(ticker.clone())
                .or_insert_with(|| ReturnsSymbolData::new(self.lookback, self.period))
                .update(now, price);
        }
    }

    /// Annualised expected returns per asset from the sample mean of returns.
    fn expected_returns(&self, returns: &[Vec<f64>]) -> Vec<f64> {
        column_means(returns)
            .iter()
            .map(|r| (1.0 + r).powf(252.0) - 1.0)
            .collect()
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

        self.update_prices(prices);

        // Deduplicated ordered ticker list from active insights.
        let mut seen = std::collections::HashSet::new();
        let tickers: Vec<String> = insights
            .iter()
            .filter(|i| seen.insert(i.symbol.value.to_string()))
            .map(|i| i.symbol.value.to_string())
            .collect();

        // Build returns matrix; skip until enough history is available.
        let returns = match form_returns_matrix(&self.asset_data, &tickers) {
            Some(r) if r.len() >= 2 => r,
            _ => return vec![],
        };

        // Annualised expected returns and covariance.
        let mu = self.expected_returns(&returns);
        let sigma = mat_scale(&covariance_matrix(&returns), 252.0);

        let mut weights = mean_variance_weights(&mu, &sigma, self.risk_free_rate);

        // Don't trust the optimizer: zero out weights with the wrong sign for
        // the requested bias (mirrors C# optimizer boundary constraints).
        for w in weights.iter_mut() {
            match self.portfolio_bias {
                PortfolioBias::Long if *w < 0.0 => *w = 0.0,
                PortfolioBias::Short if *w > 0.0 => *w = 0.0,
                _ => {}
            }
        }

        insights
            .iter()
            .filter_map(|insight| {
                let idx = tickers
                    .iter()
                    .position(|t| t.as_str() == insight.symbol.value.as_ref())?;
                let w = weights[idx];
                let pct = Decimal::try_from(w).ok()?;
                let price = prices
                    .get(insight.symbol.value.as_ref())
                    .copied()
                    .unwrap_or(Decimal::ZERO);
                Some(PortfolioTarget::percent(
                    insight.symbol.clone(),
                    pct,
                    portfolio_value,
                    price,
                ))
            })
            .collect()
    }

    fn name(&self) -> &str {
        "MaximumSharpeRatioPortfolioConstructionModel"
    }

    fn update_security_prices(&mut self, prices: &HashMap<String, Decimal>) {
        self.update_prices(prices);
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.asset_data.remove(sym.value.as_ref());
        }
    }
}
