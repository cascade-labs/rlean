/// Black-Litterman Optimization Portfolio Construction Model.
///
/// Combines market equilibrium returns (from CAPM implied returns) with
/// investor views (from alpha insights) to produce posterior expected returns,
/// then optimizes via Maximum Sharpe Ratio.
///
/// References:
///   - He, G. and Litterman, R. (1999). The intuition behind Black-Litterman model portfolios.
///   - http://www.blacklitterman.org/cookbook.html
///   - C# LEAN BlackLittermanOptimizationPortfolioConstructionModel.cs
use std::collections::{HashMap, VecDeque};

use lean_core::Symbol;
use rust_decimal::Decimal;

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

use super::matrix::{
    covariance_matrix, diag, dot, mat_add, mat_inv, mat_mul, mat_scale, mat_sub, mat_vec_mul,
    transpose, vec_add, vec_scale, vec_sub,
};

// ─── Rolling returns window ───────────────────────────────────────────────────

/// Maintains a rolling window of daily returns for a single asset.
struct AssetReturns {
    prices: VecDeque<f64>,
    lookback: usize,
    period: usize,
}

impl AssetReturns {
    fn new(lookback: usize, period: usize) -> Self {
        // We need `lookback + period` prices to compute `period` lookback-step returns.
        Self {
            prices: VecDeque::with_capacity(lookback + period + 1),
            lookback,
            period,
        }
    }

    fn push_price(&mut self, price: f64) {
        self.prices.push_back(price);
        let max_len = self.lookback + self.period + 1;
        while self.prices.len() > max_len {
            self.prices.pop_front();
        }
    }

    /// Returns `period` rate-of-change values spaced `lookback` bars apart,
    /// or None if not enough data.
    fn returns(&self) -> Option<Vec<f64>> {
        if self.prices.len() < self.lookback + 1 {
            return None;
        }
        let prices: Vec<f64> = self.prices.iter().copied().collect();
        let n = prices.len();
        // Compute returns: r[i] = (price[i + lookback] / price[i]) - 1
        // for i from 0 up to n - lookback - 1, then take the last `period` of them.
        let mut rets: Vec<f64> = Vec::new();
        for i in 0..=(n.saturating_sub(self.lookback + 1)) {
            let r = prices[i + self.lookback] / prices[i] - 1.0;
            rets.push(r);
        }
        if rets.len() < self.period {
            None
        } else {
            // Take the last `period` returns
            let start = rets.len().saturating_sub(self.period);
            Some(rets[start..].to_vec())
        }
    }
}

// ─── Black-Litterman PCM ──────────────────────────────────────────────────────

/// Portfolio bias enumeration (mirrors C# PortfolioBias).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioBias {
    /// Allow both long and short positions.
    LongShort,
    /// Long positions only (weights >= 0).
    Long,
    /// Short positions only (weights <= 0).
    Short,
}

/// Black-Litterman Optimization Portfolio Construction Model.
///
/// Parameters (all match C# defaults):
/// - `lookback`: ROC lookback (default 1 bar)
/// - `period`: rolling window length (default 63 bars ≈ 1 quarter)
/// - `risk_free_rate`: rf for Sharpe (default 0.0)
/// - `delta`: market risk-aversion coefficient δ (default 2.5)
/// - `tau`: uncertainty-in-prior scalar τ (default 0.05)
/// - `portfolio_bias`: Long/Short/LongShort (default LongShort)
pub struct BlackLittermanOptimizationPortfolioConstructionModel {
    lookback: usize,
    period: usize,
    risk_free_rate: f64,
    delta: f64,
    tau: f64,
    portfolio_bias: PortfolioBias,
    /// Per-symbol rolling price history.
    asset_data: HashMap<String, AssetReturns>,
}

impl BlackLittermanOptimizationPortfolioConstructionModel {
    /// Create with default parameters (matches C# defaults).
    pub fn new() -> Self {
        Self::with_params(1, 63, 0.0, 2.5, 0.05, PortfolioBias::LongShort)
    }

    pub fn with_params(
        lookback: usize,
        period: usize,
        risk_free_rate: f64,
        delta: f64,
        tau: f64,
        portfolio_bias: PortfolioBias,
    ) -> Self {
        Self {
            lookback,
            period,
            risk_free_rate,
            delta,
            tau,
            portfolio_bias,
            asset_data: HashMap::new(),
        }
    }

    /// Update rolling prices from the current price map.
    fn update_prices(&mut self, prices: &HashMap<String, Decimal>) {
        for (ticker, price_dec) in prices {
            let price: f64 = price_dec.to_string().parse().unwrap_or(0.0);
            if price <= 0.0 {
                continue;
            }
            self.asset_data
                .entry(ticker.clone())
                .or_insert_with(|| AssetReturns::new(self.lookback, self.period))
                .push_price(price);
        }
    }

    /// Build the returns matrix (rows = time, cols = assets) for the ordered
    /// list of tickers.  Returns None if any asset lacks enough data.
    fn build_returns_matrix(&self, tickers: &[String]) -> Option<Vec<Vec<f64>>> {
        let per_asset: Vec<Vec<f64>> = tickers
            .iter()
            .map(|t| {
                self.asset_data
                    .get(t)
                    .and_then(|d| d.returns())
                    .unwrap_or_default()
            })
            .collect();

        // All assets must have the same positive number of returns.
        let n_rows = per_asset.iter().map(|v| v.len()).min().unwrap_or(0);
        if n_rows == 0 {
            return None;
        }

        // Build row-major matrix: rows = time, cols = asset
        let n_cols = tickers.len();
        let matrix: Vec<Vec<f64>> = (0..n_rows)
            .map(|t| (0..n_cols).map(|c| per_asset[c][t]).collect())
            .collect();

        Some(matrix)
    }

    /// Compute equilibrium returns π = δ × Σ × w
    /// and the annualised covariance Σ.
    ///
    /// Mirrors C# GetEquilibriumReturns: equal-weight w, annualises by 252.
    fn equilibrium_returns(&self, returns: &[Vec<f64>]) -> (Vec<f64>, Vec<Vec<f64>>) {
        let n = returns[0].len();
        let w = vec![1.0 / n as f64; n]; // equal weights

        // Annualised sample covariance
        let cov = covariance_matrix(returns);
        let sigma = mat_scale(&cov, 252.0);

        // Annualised return per asset: geometric mean approx
        let t = returns.len() as f64;
        let mean_daily: Vec<f64> = (0..n)
            .map(|j| returns.iter().map(|row| row[j]).sum::<f64>() / t)
            .collect();
        let ann_return: Vec<f64> = mean_daily
            .iter()
            .map(|r| (1.0 + r).powf(252.0) - 1.0)
            .collect();

        // Annual portfolio return and variance
        let port_return: f64 = dot(&w, &ann_return);
        let sigma_w = mat_vec_mul(&sigma, &w);
        let port_variance: f64 = dot(&w, &sigma_w);

        // Risk aversion from Sharpe decomposition
        let risk_aversion = if port_variance.abs() < 1e-12 {
            self.delta
        } else {
            (port_return - self.risk_free_rate) / port_variance
        };

        // π = risk_aversion × Σ × w
        let pi = vec_scale(&sigma_w, risk_aversion);

        (pi, sigma)
    }

    /// Apply the Black-Litterman master formula.
    ///
    /// Updates π (posterior mean) and Σ (posterior covariance) in-place.
    ///
    /// Formula:
    ///   Ω  = diag(τ × P × Σ × Pᵀ)            uncertainty of views
    ///   A  = τΣ × Pᵀ × (P × τΣ × Pᵀ + Ω)⁻¹
    ///   π* = π + A × (Q - P × π)               posterior mean
    ///   M  = τΣ - A × P × τΣ                   posterior uncertainty
    ///   Σ* = (Σ + M) × δ                        scaled posterior covariance
    fn apply_master_formula(
        &self,
        pi: &[f64],
        sigma: &[Vec<f64>],
        p: &[Vec<f64>], // K×N view matrix
        q: &[f64],      // K view returns
    ) -> Option<(Vec<f64>, Vec<Vec<f64>>)> {
        let _n = sigma.len();
        let k = p.len();

        let tau = self.tau;
        let sigma_tau = mat_scale(sigma, tau); // τΣ (N×N)

        // Ω = diag(P × τΣ × Pᵀ) element-wise, i.e. P(τΣ)Pᵀ ⊙ I_k
        let p_sigma_tau = mat_mul(p, &sigma_tau); // K×N
        let pt = transpose(p); // N×K
        let p_sigma_tau_pt = mat_mul(&p_sigma_tau, &pt); // K×K

        // Ω is diagonal: Ω_ii = P(τΣ)Pᵀ_ii
        let omega_diag: Vec<f64> = (0..k).map(|i| p_sigma_tau_pt[i][i]).collect();
        let omega = diag(&omega_diag); // K×K

        // Check Ω is non-singular
        // Check Ω is non-singular by attempting inversion; we don't use omega_inv directly
        // but need to verify the matrix is invertible before proceeding.
        mat_inv(&omega)?;

        // A = τΣ × Pᵀ × (P × τΣ × Pᵀ + Ω)⁻¹
        let denom = mat_add(&p_sigma_tau_pt, &omega); // K×K
        let denom_inv = mat_inv(&denom)?; // K×K
        let sigma_tau_pt = mat_mul(&sigma_tau, &pt); // N×K
        let a = mat_mul(&sigma_tau_pt, &denom_inv); // N×K

        // posterior mean: π* = π + A × (Q - P × π)
        let p_pi = mat_vec_mul(p, pi); // K
        let q_minus_p_pi = vec_sub(q, &p_pi); // K
        let a_times_diff = mat_vec_mul(&a, &q_minus_p_pi); // N
        let pi_post = vec_add(pi, &a_times_diff); // N

        // posterior uncertainty: M = τΣ - A × P × τΣ
        let a_p = mat_mul(&a, p); // N×N
        let a_p_sigma_tau = mat_mul(&a_p, &sigma_tau); // N×N
        let m = mat_sub(&sigma_tau, &a_p_sigma_tau); // N×N

        // scaled posterior covariance: Σ* = (Σ + M) × δ
        let sigma_post = mat_scale(&mat_add(sigma, &m), self.delta); // N×N

        // Validate: check for NaN/Inf
        if pi_post.iter().any(|v| !v.is_finite()) {
            return None;
        }

        Some((pi_post, sigma_post))
    }

    /// Maximum Sharpe Ratio optimization given posterior returns and covariance.
    ///
    /// Analytical solution with diagonal covariance approximation:
    ///   w_i ∝ (μ_i - rf) / σ_i²
    ///
    /// For the full covariance case (used here), we approximate via the
    /// tangency portfolio: w ∝ Σ⁻¹ × (μ - rf × 1).
    /// Falls back to equal weights if Σ is singular.
    fn max_sharpe_optimize(&self, mu: &[f64], sigma: &[Vec<f64>], n: usize) -> Vec<f64> {
        let rf = self.risk_free_rate;
        let excess: Vec<f64> = mu.iter().map(|m| m - rf).collect();

        let weights = if let Some(sigma_inv) = mat_inv(sigma) {
            mat_vec_mul(&sigma_inv, &excess)
        } else {
            // Fallback: diagonal approximation
            sigma
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let var = row[i];
                    if var.abs() < 1e-12 {
                        0.0
                    } else {
                        excess[i] / var
                    }
                })
                .collect()
        };

        // Apply portfolio bias constraints
        let constrained: Vec<f64> = weights
            .iter()
            .map(|&w| match self.portfolio_bias {
                PortfolioBias::Long => w.max(0.0),
                PortfolioBias::Short => w.min(0.0),
                PortfolioBias::LongShort => w,
            })
            .collect();

        // Normalize by absolute sum so portfolio is fully invested
        let abs_sum: f64 = constrained.iter().map(|w| w.abs()).sum();
        if abs_sum < 1e-12 {
            // Degenerate: equal weights
            vec![1.0 / n as f64; n]
        } else {
            constrained.iter().map(|w| w / abs_sum).collect()
        }
    }

    /// Build P matrix and Q vector from insights grouped by source model.
    ///
    /// Each source model contributes one row to P and one element to Q.
    /// This matches C# TryGetViews logic.
    fn build_views(
        &self,
        insights: &[InsightForPcm],
        tickers: &[String],
    ) -> Option<(Vec<Vec<f64>>, Vec<f64>)> {
        // Group insights by source model
        let mut groups: HashMap<String, Vec<&InsightForPcm>> = HashMap::new();
        for insight in insights {
            groups
                .entry(insight.source_model.clone())
                .or_default()
                .push(insight);
        }

        let ticker_index: HashMap<&str, usize> = tickers
            .iter()
            .enumerate()
            .map(|(i, t)| (t.as_str(), i))
            .collect();

        let n = tickers.len();
        let mut p_rows: Vec<Vec<f64>> = Vec::new();
        let mut q_vec: Vec<f64> = Vec::new();

        for group in groups.values() {
            // Compute Q for this group: max of up-magnitude-sum vs down-magnitude-sum
            let up_sum: f64 = group
                .iter()
                .filter(|i| i.direction == InsightDirection::Up)
                .filter_map(|i| {
                    i.magnitude
                        .map(|m| m.abs().to_string().parse::<f64>().ok().unwrap_or(0.0))
                })
                .sum();
            let dn_sum: f64 = group
                .iter()
                .filter(|i| i.direction == InsightDirection::Down)
                .filter_map(|i| {
                    i.magnitude
                        .map(|m| m.abs().to_string().parse::<f64>().ok().unwrap_or(0.0))
                })
                .sum();

            let q_val = if up_sum >= dn_sum { up_sum } else { dn_sum };
            if q_val == 0.0 {
                continue;
            }

            // Build P row: each asset's weighted contribution
            let mut p_row = vec![0.0; n];
            for insight in group.iter() {
                if let Some(&idx) = ticker_index.get(insight.symbol.value.as_str()) {
                    let mag: f64 = insight
                        .magnitude
                        .map(|m| m.abs().to_string().parse().unwrap_or(0.0))
                        .unwrap_or(0.0);
                    let direction = insight.direction.as_i32() as f64;
                    p_row[idx] = direction * mag / q_val;
                }
            }

            // Skip degenerate rows (all zeros)
            let row_sum: f64 = p_row.iter().map(|v| v.abs()).sum();
            if row_sum < 1e-12 {
                continue;
            }

            p_rows.push(p_row);
            q_vec.push(q_val);
        }

        if p_rows.is_empty() {
            None
        } else {
            Some((p_rows, q_vec))
        }
    }
}

impl Default for BlackLittermanOptimizationPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for BlackLittermanOptimizationPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        if insights.is_empty() {
            return vec![];
        }

        // Update rolling price history
        self.update_prices(prices);

        // Collect ordered ticker list from active insights (deduplicated)
        let mut seen = std::collections::HashSet::new();
        let tickers: Vec<String> = insights
            .iter()
            .filter(|i| seen.insert(i.symbol.value.clone()))
            .map(|i| i.symbol.value.clone())
            .collect();

        let n = tickers.len();

        // Build returns matrix; return empty if not enough history
        let returns = match self.build_returns_matrix(&tickers) {
            Some(r) if r.len() >= 2 => r,
            _ => return vec![],
        };

        // Compute equilibrium returns and covariance
        let (mut pi, mut sigma) = self.equilibrium_returns(&returns);

        // Build views from insights
        if let Some((p, q)) = self.build_views(insights, &tickers) {
            // Apply Black-Litterman master formula
            if let Some((pi_post, sigma_post)) = self.apply_master_formula(&pi, &sigma, &p, &q) {
                pi = pi_post;
                sigma = sigma_post;
            }
            // If formula fails (singular Ω etc.), use equilibrium as fallback
        }

        // Optimize: Maximum Sharpe Ratio
        let weights = self.max_sharpe_optimize(&pi, &sigma, n);

        // Build portfolio targets
        insights
            .iter()
            .filter_map(|insight| {
                let idx = tickers.iter().position(|t| *t == insight.symbol.value)?;
                let w = weights[idx];
                let pct = Decimal::try_from(w).ok()?;
                let price = prices
                    .get(&insight.symbol.value)
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

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.asset_data.remove(&sym.value);
        }
    }

    fn update_security_prices(
        &mut self,
        prices: &std::collections::HashMap<String, rust_decimal::Decimal>,
    ) {
        self.update_prices(prices);
    }

    fn name(&self) -> &str {
        "BlackLittermanOptimizationPortfolioConstructionModel"
    }
}
