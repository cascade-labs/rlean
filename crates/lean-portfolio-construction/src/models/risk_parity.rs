/// Risk Parity Portfolio Construction Model.
///
/// Equalizes the risk contribution of each asset in the portfolio.
/// Risk contribution of asset i: RC_i = w_i × (Σw)_i / √(wᵀΣw)
/// Target: all RC_i = 1/N (equal risk budget).
///
/// Uses Newton's method to solve the convex optimization problem:
///   minimize f(x) = ½ xᵀΣx - bᵀlog(x)
///   where b_i = 1/N (equal risk budget)
///
/// References:
///   Spinu, F. (2013). An algorithm for computing risk parity weights.
///   SSRN 2297383. https://papers.ssrn.com/sol3/Papers.cfm?abstract_id=2297383
///
///   C# LEAN RiskParityPortfolioConstructionModel.cs / RiskParityPortfolioOptimizer.cs
use std::collections::HashMap;

use lean_core::Symbol;
use rust_decimal::Decimal;

use crate::portfolio_construction_model::{IPortfolioConstructionModel, InsightForPcm};
use crate::portfolio_target::PortfolioTarget;

use super::matrix::{covariance_matrix, diag, dot, mat_add, mat_inv, mat_vec_mul};

// ─── Risk-parity Newton optimizer ────────────────────────────────────────────

/// Optimize weights so that each asset contributes equally to portfolio risk.
///
/// Returns normalized weights w_i >= lower_bound summing to 1.
///
/// # Parameters
/// - `cov`: N×N covariance matrix
/// - `budget`: risk budget (length N); use `1/N` for equal-risk parity
/// - `lower_bound`: minimum weight per asset (default 1e-5, must be > 0)
/// - `upper_bound`: maximum weight per asset (default f64::MAX)
/// - `tolerance`: convergence tolerance on objective change (default 1e-11)
/// - `max_iter`: maximum Newton iterations (default 15 000)
pub fn risk_parity_optimize(
    cov: &[Vec<f64>],
    budget: Option<&[f64]>,
    lower_bound: f64,
    upper_bound: f64,
    tolerance: f64,
    max_iter: usize,
) -> Vec<f64> {
    let n = cov.len();
    if n == 0 {
        return vec![];
    }
    if n == 1 {
        return vec![1.0];
    }

    let equal_budget: Vec<f64> = vec![1.0 / n as f64; n];
    let b: &[f64] = budget.unwrap_or(&equal_budget);

    // Objective: f(x) = 0.5 * xᵀΣx - bᵀlog(x)
    let objective = |x: &[f64]| -> f64 {
        let sx = mat_vec_mul(cov, x);
        let quad = dot(x, &sx);
        let log_term: f64 = b.iter().zip(x.iter()).map(|(bi, xi)| bi * xi.ln()).sum();
        0.5 * quad - log_term
    };

    // Gradient: df/dx = Σx - b/x
    let gradient = |x: &[f64]| -> Vec<f64> {
        let sx = mat_vec_mul(cov, x);
        sx.iter()
            .zip(b.iter().zip(x.iter()))
            .map(|(sxi, (bi, xi))| sxi - bi / xi)
            .collect()
    };

    // Hessian: H(x) = Σ + diag(b/x²)
    let hessian = |x: &[f64]| -> Vec<Vec<f64>> {
        let diag_vec: Vec<f64> = b
            .iter()
            .zip(x.iter())
            .map(|(bi, xi)| bi / (xi * xi))
            .collect();
        let d = diag(&diag_vec);
        mat_add(cov, &d)
    };

    // Initialize with equal weights
    let mut w: Vec<f64> = vec![1.0 / n as f64; n];
    let mut new_obj = f64::MIN;
    let mut old_obj = f64::MAX;
    let mut iter = 0;

    while (new_obj - old_obj).abs() > tolerance && iter < max_iter {
        old_obj = new_obj;

        let h = hessian(&w);
        let g = gradient(&w);

        // Newton step: w ← w - H⁻¹ g
        if let Some(h_inv) = mat_inv(&h) {
            let delta = mat_vec_mul(&h_inv, &g);
            for i in 0..n {
                w[i] -= delta[i];
                // Keep strictly positive so log(w) is defined
                if w[i] <= 1e-15 {
                    w[i] = 1e-15;
                }
            }
        } else {
            // Hessian singular: fall back to gradient descent
            let step = 0.01;
            for i in 0..n {
                w[i] -= step * g[i];
                if w[i] <= 1e-15 {
                    w[i] = 1e-15;
                }
            }
        }

        new_obj = objective(&w);
        iter += 1;
    }

    // Normalize: w = w / sum(w)
    let total: f64 = w.iter().sum();
    if total < 1e-12 {
        return vec![1.0 / n as f64; n];
    }
    let normalized: Vec<f64> = w.iter().map(|x| x / total).collect();

    // Clamp to [lower_bound, upper_bound]
    normalized
        .into_iter()
        .map(|x| x.clamp(lower_bound, upper_bound))
        .collect()
}

// ─── Rolling prices ───────────────────────────────────────────────────────────

struct AssetPrices {
    prices: std::collections::VecDeque<f64>,
    lookback: usize,
    period: usize,
}

impl AssetPrices {
    fn new(lookback: usize, period: usize) -> Self {
        Self {
            prices: std::collections::VecDeque::with_capacity(lookback + period + 1),
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

    fn returns(&self) -> Option<Vec<f64>> {
        if self.prices.len() < self.lookback + 1 {
            return None;
        }
        let prices: Vec<f64> = self.prices.iter().copied().collect();
        let n = prices.len();
        let mut rets = Vec::new();
        for i in 0..=(n.saturating_sub(self.lookback + 1)) {
            let r = prices[i + self.lookback] / prices[i] - 1.0;
            rets.push(r);
        }
        if rets.len() < self.period {
            None
        } else {
            let start = rets.len().saturating_sub(self.period);
            Some(rets[start..].to_vec())
        }
    }
}

// ─── Risk Parity PCM ──────────────────────────────────────────────────────────

/// Risk Parity Portfolio Construction Model.
///
/// Parameters (matching C# defaults):
/// - `lookback`: ROC lookback in bars (default 1)
/// - `period`: rolling window length (default 252 bars = 1 year)
pub struct RiskParityPortfolioConstructionModel {
    lookback: usize,
    period: usize,
    lower_bound: f64,
    upper_bound: f64,
    asset_data: HashMap<String, AssetPrices>,
}

impl RiskParityPortfolioConstructionModel {
    /// Create with default parameters (matches C# defaults).
    pub fn new() -> Self {
        Self::with_params(1, 252)
    }

    pub fn with_params(lookback: usize, period: usize) -> Self {
        Self {
            lookback,
            period,
            lower_bound: 1e-5,
            upper_bound: f64::MAX,
            asset_data: HashMap::new(),
        }
    }

    /// Override the weight bounds.
    pub fn with_bounds(mut self, lower: f64, upper: f64) -> Self {
        self.lower_bound = lower;
        self.upper_bound = upper;
        self
    }

    fn update_prices(&mut self, prices: &HashMap<String, Decimal>) {
        for (ticker, price_dec) in prices {
            let price: f64 = price_dec.to_string().parse().unwrap_or(0.0);
            if price <= 0.0 {
                continue;
            }
            self.asset_data
                .entry(ticker.clone())
                .or_insert_with(|| AssetPrices::new(self.lookback, self.period))
                .push_price(price);
        }
    }

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

        let n_rows = per_asset.iter().map(|v| v.len()).min().unwrap_or(0);
        if n_rows == 0 {
            return None;
        }

        let n_cols = tickers.len();
        let matrix: Vec<Vec<f64>> = (0..n_rows)
            .map(|t| (0..n_cols).map(|c| per_asset[c][t]).collect())
            .collect();

        Some(matrix)
    }
}

impl Default for RiskParityPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for RiskParityPortfolioConstructionModel {
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

        // Deduplicated ordered ticker list
        let mut seen = std::collections::HashSet::new();
        let tickers: Vec<String> = insights
            .iter()
            .filter(|i| seen.insert(i.symbol.value.clone()))
            .map(|i| i.symbol.value.clone())
            .collect();

        let n = tickers.len();

        // Build returns matrix; skip if insufficient history
        let returns = match self.build_returns_matrix(&tickers) {
            Some(r) if r.len() >= 2 => r,
            _ => return vec![],
        };

        // Compute sample covariance
        let cov = covariance_matrix(&returns);

        // Equal risk budget b_i = 1/N
        let budget = vec![1.0 / n as f64; n];

        // Optimize
        let weights = risk_parity_optimize(
            &cov,
            Some(&budget),
            self.lower_bound,
            self.upper_bound,
            1e-11,
            15_000,
        );

        // Map weights to portfolio targets
        insights
            .iter()
            .filter_map(|insight| {
                let idx = tickers.iter().position(|t| *t == insight.symbol.value)?;
                let w = weights.get(idx).copied().unwrap_or(0.0);
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

    fn name(&self) -> &str {
        "RiskParityPortfolioConstructionModel"
    }
}

// ─── Risk contribution utilities (exposed for testing) ────────────────────────

/// Compute risk contributions for weights w given covariance Σ.
/// RC_i = w_i * (Σw)_i / portfolio_vol
/// where portfolio_vol = sqrt(wᵀΣw).
pub fn risk_contributions(w: &[f64], cov: &[Vec<f64>]) -> Vec<f64> {
    let sigma_w = mat_vec_mul(cov, w);
    let portfolio_var = dot(w, &sigma_w);
    let portfolio_vol = portfolio_var.sqrt();
    if portfolio_vol < 1e-12 {
        return vec![1.0 / w.len() as f64; w.len()];
    }
    w.iter()
        .zip(sigma_w.iter())
        .map(|(wi, sw_i)| wi * sw_i / portfolio_vol)
        .collect()
}
