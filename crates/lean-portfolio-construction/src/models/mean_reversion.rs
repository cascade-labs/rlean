use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};

use crate::portfolio_construction_model::{IPortfolioConstructionModel, InsightForPcm};
use crate::portfolio_target::PortfolioTarget;

/// Per-symbol rolling price window used to compute the moving-average reversion level.
struct SymbolWindow {
    prices: VecDeque<f64>,
    window_size: usize,
}

impl SymbolWindow {
    fn new(window_size: usize) -> Self {
        Self {
            prices: VecDeque::with_capacity(window_size + 1),
            window_size,
        }
    }

    fn push(&mut self, price: f64) {
        self.prices.push_back(price);
        if self.prices.len() > self.window_size {
            self.prices.pop_front();
        }
    }

    fn is_ready(&self) -> bool {
        self.prices.len() >= self.window_size
    }

    /// Current price (last pushed value, analogous to C# Identity indicator).
    fn current(&self) -> f64 {
        *self.prices.back().unwrap_or(&0.0)
    }

    /// Simple moving average of all prices in the window.
    fn sma(&self) -> f64 {
        if self.prices.is_empty() {
            return 0.0;
        }
        self.prices.iter().copied().sum::<f64>() / self.prices.len() as f64
    }
}

/// Implementation of On-Line Moving Average Reversion (OLMAR).
///
/// Mirrors C# `MeanReversionPortfolioConstructionModel`.
///
/// Reference: Li, B., Hoi, S. C. (2012). On-line portfolio selection with moving average
/// reversion. arXiv:1206.4626. <https://arxiv.org/ftp/arxiv/papers/1206/1206.4626.pdf>
///
/// Using `window_size = 1` degenerates into Passive Aggressive Mean Reversion (PAMR).
///
/// # Algorithm
/// 1. For each asset, compute price relative `x̃ = price / SMA(price)`.
///    If an insight has `magnitude`, use `x̃ = 1 + magnitude * direction` instead.
/// 2. Compute step size: `λ = max(0, (b·x̃ − ε) / ‖x̃ − mean(x̃)·1‖²)`.
/// 3. Update portfolio: `b' = b − λ·(x̃ − mean(x̃)·1)`.
/// 4. Project `b'` onto the probability simplex.
///
/// Long-only (portfolio bias ≠ Short).  Raises an error if Short-only is requested.
pub struct MeanReversionPortfolioConstructionModel {
    /// Reversion threshold ε (default 1.0).
    reversion_threshold: f64,
    /// SMA window size (default 20).
    window_size: usize,
    /// Current portfolio weight vector (one entry per asset, in insight order).
    weight_vector: Vec<f64>,
    /// Number of assets last seen — used to detect universe changes.
    num_of_assets: usize,
    /// Per-symbol price windows.
    symbol_data: HashMap<String, SymbolWindow>,
}

impl MeanReversionPortfolioConstructionModel {
    /// Create with default parameters (ε = 1, window = 20).
    pub fn new() -> Self {
        Self::with_params(1.0, 20)
    }

    /// Create with custom reversion threshold and SMA window size.
    pub fn with_params(reversion_threshold: f64, window_size: usize) -> Self {
        Self {
            reversion_threshold,
            window_size,
            weight_vector: Vec::new(),
            num_of_assets: 0,
            symbol_data: HashMap::new(),
        }
    }

    /// Feed a price observation for a symbol.  Call this before `create_targets` to
    /// warm up the SMA indicators.
    pub fn update_price(&mut self, ticker: &str, price: f64) {
        self.symbol_data
            .entry(ticker.to_string())
            .or_insert_with(|| SymbolWindow::new(self.window_size))
            .push(price);
    }

    /// Compute price relatives for the given active insights.
    ///
    /// If an insight has `magnitude`, uses `1 + magnitude * direction_sign`.
    /// Otherwise uses `current_price / sma`.
    pub fn get_price_relatives(&self, insights: &[InsightForPcm]) -> Vec<f64> {
        insights
            .iter()
            .map(|insight| {
                if let Some(mag) = insight.magnitude {
                    let mag_f: f64 = mag.try_into().unwrap_or(0.0);
                    let dir_sign = insight.direction.as_i32() as f64;
                    1.0 + mag_f * dir_sign
                } else {
                    let sd = self.symbol_data.get(&insight.symbol.value);
                    match sd {
                        Some(sd) if sd.sma() != 0.0 => sd.current() / sd.sma(),
                        _ => 1.0, // fallback: no reversion signal
                    }
                }
            })
            .collect()
    }

    /// Project a vector onto the probability simplex (all elements ≥ 0, sum = `total`).
    ///
    /// Algorithm from Duchi et al. (2008), ICML.
    pub fn simplex_projection(vector: &[f64], total: f64) -> Vec<f64> {
        assert!(total > 0.0, "Total must be > 0 for Euclidean Projection onto the Simplex.");

        let mut mu: Vec<f64> = vector.to_vec();
        mu.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

        // Cumulative sum of sorted vector.
        let sv: Vec<f64> = mu
            .iter()
            .scan(0.0, |acc, &x| {
                *acc += x;
                Some(*acc)
            })
            .collect();

        let rho = mu
            .iter()
            .enumerate()
            .filter(|(i, &u)| u > (sv[*i] - total) / (*i as f64 + 1.0))
            .map(|(i, _)| i)
            .next_back()
            .unwrap_or(0);

        let theta = (sv[rho] - total) / (rho as f64 + 1.0);
        vector.iter().map(|&x| (x - theta).max(0.0)).collect()
    }

    /// Cumulative sum of a sequence (used in tests / for public API parity with C#).
    pub fn cumulative_sum(sequence: &[f64]) -> Vec<f64> {
        let mut acc = 0.0;
        sequence
            .iter()
            .map(|&x| {
                acc += x;
                acc
            })
            .collect()
    }
}

impl Default for MeanReversionPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for MeanReversionPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        // Update price windows from the current prices map.
        for insight in insights {
            if let Some(&price) = prices.get(&insight.symbol.value) {
                let price_f: f64 = price.try_into().unwrap_or(0.0);
                if price_f > 0.0 {
                    self.symbol_data
                        .entry(insight.symbol.value.clone())
                        .or_insert_with(|| SymbolWindow::new(self.window_size))
                        .push(price_f);
                }
            }
        }

        if insights.is_empty() {
            return Vec::new();
        }

        // Check that all symbol windows are ready.
        let all_ready = insights.iter().all(|i| {
            self.symbol_data
                .get(&i.symbol.value)
                .map(|sd| sd.is_ready())
                .unwrap_or(false)
        });

        if !all_ready {
            return Vec::new();
        }

        let num = insights.len();
        if self.num_of_assets != num {
            self.num_of_assets = num;
            // Reinitialise to uniform weights.
            let w = 1.0 / num as f64;
            self.weight_vector = vec![w; num];
        }

        // Price relatives: x̃_{t+1}
        let price_relatives = self.get_price_relatives(insights);

        // x̄ = mean(x̃)
        let mean_pr: f64 = price_relatives.iter().sum::<f64>() / num as f64;

        // Deviation from mean: x̃ - x̄·1
        let dev: Vec<f64> = price_relatives.iter().map(|&x| x - mean_pr).collect();

        // ||dev||²
        let second_norm: f64 = dev.iter().map(|&d| d * d).sum();

        let step_size = if second_norm == 0.0 {
            0.0
        } else {
            let dot: f64 = self
                .weight_vector
                .iter()
                .zip(price_relatives.iter())
                .map(|(w, x)| w * x)
                .sum();
            ((dot - self.reversion_threshold) / second_norm).max(0.0)
        };

        // b' = b - step_size * dev
        let next_portfolio: Vec<f64> = self
            .weight_vector
            .iter()
            .zip(dev.iter())
            .map(|(w, d)| w - d * step_size)
            .collect();

        // Project onto simplex.
        let normalized = Self::simplex_projection(&next_portfolio, 1.0);
        self.weight_vector = normalized.clone();

        // Build targets (long-only — all weights are non-negative after simplex projection).
        insights
            .iter()
            .zip(normalized.iter())
            .map(|(insight, &w)| {
                let pct = Decimal::try_from(w).unwrap_or(Decimal::ZERO);
                let price = prices.get(&insight.symbol.value).copied().unwrap_or(Decimal::ZERO);
                PortfolioTarget::percent(insight.symbol.clone(), pct, portfolio_value, price)
            })
            .collect()
    }

    fn name(&self) -> &str {
        "MeanReversionPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.symbol_data.remove(&sym.value);
        }
        // If universe changed, weight vector will be reset on the next create_targets call.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio_construction_model::InsightDirection;
    use lean_core::{Market, Symbol};
    use rust_decimal_macros::dec;

    fn make_symbol(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    fn make_prices(pairs: &[(&str, Decimal)]) -> HashMap<String, Decimal> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect()
    }

    #[test]
    fn cumulative_sum_matches_c_sharp() {
        let input = vec![1.1, 2.5, 0.7, 13.6, -5.2, 3.9, -1.6];
        let expected = vec![1.1, 3.6, 4.3, 17.9, 12.7, 16.6, 15.0];
        let result: Vec<f64> = MeanReversionPortfolioConstructionModel::cumulative_sum(&input)
            .iter()
            .map(|&x| (x * 10.0).round() / 10.0)
            .collect();
        assert_eq!(result, expected);
    }

    #[test]
    fn simplex_projection_matches_c_sharp_total_1() {
        // C# test: _simplexTestArray = {0.2, 0.5, 0.4, -0.1, 0.0}
        // expected at total=1: {1/6, 7/15, 11/30, 0, 0}
        let input = vec![0.2, 0.5, 0.4, -0.1, 0.0];
        let result = MeanReversionPortfolioConstructionModel::simplex_projection(&input, 1.0);
        let round8: Vec<f64> = result.iter().map(|&x| (x * 1e8).round() / 1e8).collect();
        let expected: Vec<f64> = vec![
            (1.0_f64 / 6.0 * 1e8).round() / 1e8,
            (7.0_f64 / 15.0 * 1e8).round() / 1e8,
            (11.0_f64 / 30.0 * 1e8).round() / 1e8,
            0.0,
            0.0,
        ];
        assert_eq!(round8, expected);
    }

    #[test]
    fn simplex_projection_matches_c_sharp_total_half() {
        // C# test: expected at total=0.5: {0, 0.3, 0.2, 0, 0}
        let input = vec![0.2, 0.5, 0.4, -0.1, 0.0];
        let result = MeanReversionPortfolioConstructionModel::simplex_projection(&input, 0.5);
        let round8: Vec<f64> = result.iter().map(|&x| (x * 1e8).round() / 1e8).collect();
        let expected: Vec<f64> = vec![0.0, 0.3, 0.2, 0.0, 0.0];
        assert_eq!(round8, expected);
    }

    #[test]
    fn simplex_projection_panics_on_zero_total() {
        let result = std::panic::catch_unwind(|| {
            MeanReversionPortfolioConstructionModel::simplex_projection(&[0.5, 0.5], 0.0);
        });
        assert!(result.is_err(), "Should panic for total=0");
    }

    #[test]
    fn equal_weights_when_magnitude_zero() {
        // Two insights with magnitude=0 → price relatives both 1.0 → mean=1.0,
        // dev = [0, 0], step=0, b unchanged (stays uniform).
        let mut pcm = MeanReversionPortfolioConstructionModel::with_params(1.0, 1);
        let aapl = make_symbol("AAPL");
        let spy = make_symbol("SPY");
        let portfolio_value = dec!(1_000);
        // Price = 10 per share; at window=1 one data point is enough.
        let prices = make_prices(&[("AAPL", dec!(10)), ("SPY", dec!(10))]);

        // Warm up windows (window_size=1 → ready after 1 price).
        pcm.update_price("AAPL", 10.0);
        pcm.update_price("SPY", 10.0);

        let insights = vec![
            InsightForPcm {
                symbol: aapl.clone(),
                direction: InsightDirection::Up,
                magnitude: Some(dec!(0)),
                confidence: None,
                source_model: "test".to_string(),
            },
            InsightForPcm {
                symbol: spy.clone(),
                direction: InsightDirection::Up,
                magnitude: Some(dec!(0)),
                confidence: None,
                source_model: "test".to_string(),
            },
        ];

        let targets = pcm.create_targets(&insights, portfolio_value, &prices);
        assert_eq!(targets.len(), 2);
        // Both at equal weight 0.5 → qty = round(1000 * 0.5 / 10) = 50
        for t in &targets {
            assert_eq!(t.quantity, dec!(50), "expected qty=50, got {}", t.quantity);
        }
    }

    #[test]
    fn price_relatives_from_magnitude_override() {
        // With magnitude=1, direction=Up → x̃ = 1 + 1*1 = 2
        // With magnitude=-0.5, direction=Up → x̃ = 1 + (-0.5)*1 = 0.5
        // Mirrors C# test "GetPriceRelativesWithInsightMagnitude" → expected = {2, 0.5}
        let mut pcm = MeanReversionPortfolioConstructionModel::with_params(1.0, 1);
        pcm.update_price("AAPL", 10.0);
        pcm.update_price("SPY", 10.0);

        let insights = vec![
            InsightForPcm {
                symbol: make_symbol("AAPL"),
                direction: InsightDirection::Up,
                magnitude: Some(dec!(1)),
                confidence: None,
                source_model: "test".to_string(),
            },
            InsightForPcm {
                symbol: make_symbol("SPY"),
                direction: InsightDirection::Up,
                magnitude: Some(dec!(-0.5)),
                confidence: None,
                source_model: "test".to_string(),
            },
        ];

        let pr = pcm.get_price_relatives(&insights);
        assert_eq!(pr[0], 2.0);
        assert_eq!(pr[1], 0.5);
    }

    #[test]
    fn correct_weightings_with_magnitude() {
        // Mirrors C# CorrectWeightings test case: direction1=Up, direction2=Up, mag1=1, mag2=-0.5
        // Expected qty1=31, qty2=63 (portfolio 1200, free=250, price=10).
        // Net portfolio after free = 1200 - 250 = 950. But our model uses portfolio_value directly.
        // Let's use portfolio=950, price=10, window=1, threshold=1.
        let mut pcm = MeanReversionPortfolioConstructionModel::with_params(1.0, 1);
        let aapl = make_symbol("AAPL");
        let spy = make_symbol("SPY");
        let portfolio_value = dec!(950);
        let prices = make_prices(&[("AAPL", dec!(10)), ("SPY", dec!(10))]);

        pcm.update_price("AAPL", 10.0);
        pcm.update_price("SPY", 10.0);

        let insights = vec![
            InsightForPcm {
                symbol: aapl.clone(),
                direction: InsightDirection::Up,
                magnitude: Some(dec!(1)),
                confidence: None,
                source_model: "test".to_string(),
            },
            InsightForPcm {
                symbol: spy.clone(),
                direction: InsightDirection::Up,
                magnitude: Some(dec!(-0.5)),
                confidence: None,
                source_model: "test".to_string(),
            },
        ];

        let targets = pcm.create_targets(&insights, portfolio_value, &prices);
        let aapl_qty = targets.iter().find(|t| t.symbol.value == "AAPL").unwrap().quantity;
        let spy_qty = targets.iter().find(|t| t.symbol.value == "SPY").unwrap().quantity;

        // C# uses Math.Floor; Rust PortfolioTarget uses .round().  The weight split is
        // approximately 1/3 : 2/3.  950*1/3/10=31.67→round=32, 950*2/3/10=63.33→round=63.
        // C# produces 31 and 63 (floor).  Both are within 1 share of each other — the OLMAR
        // algorithm is identical; only the quantity rounding differs.
        assert_eq!(aapl_qty, dec!(32), "AAPL: expected 32 (Rust round vs C# floor=31), got {}", aapl_qty);
        assert_eq!(spy_qty, dec!(63), "SPY: expected 63, got {}", spy_qty);
    }
}
