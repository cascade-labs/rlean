use std::collections::{HashMap, VecDeque};

use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use rust_decimal::Decimal;

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

// ---------------------------------------------------------------------------
// Pearson correlation helper
// ---------------------------------------------------------------------------

/// Compute the Pearson product-moment correlation coefficient for two equal-length
/// slices.  Returns `None` when n < 2 or when either series has zero variance.
fn pearson_correlation(x: &[f64], y: &[f64]) -> Option<f64> {
    let n = x.len();
    if n < 2 || n != y.len() {
        return None;
    }

    let n_f = n as f64;
    let mean_x = x.iter().sum::<f64>() / n_f;
    let mean_y = y.iter().sum::<f64>() / n_f;

    let (cov, var_x, var_y) =
        x.iter()
            .zip(y.iter())
            .fold((0.0_f64, 0.0_f64, 0.0_f64), |(cov, vx, vy), (&xi, &yi)| {
                let dx = xi - mean_x;
                let dy = yi - mean_y;
                (cov + dx * dy, vx + dx * dx, vy + dy * dy)
            });

    if var_x == 0.0 || var_y == 0.0 {
        return None;
    }

    Some(cov / (var_x.sqrt() * var_y.sqrt()))
}

// ---------------------------------------------------------------------------
// Per-pair spread state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairState {
    Flat,
    LongRatio,  // ratio > upper threshold → short asset1, long asset2
    ShortRatio, // ratio < lower threshold → long asset1, short asset2
}

struct PairData {
    state: PairState,
    /// EMA-500 running state (decay factor α = 2/(500+1))
    ema: Option<f64>,
    ema_n: usize, // warm-up counter
}

impl PairData {
    fn new() -> Self {
        Self {
            state: PairState::Flat,
            ema: None,
            ema_n: 0,
        }
    }

    /// Update with the latest ratio = price_a / price_b.
    /// Returns the current state after applying the threshold check.
    fn update(&mut self, ratio: f64, threshold_pct: f64) -> PairState {
        const EMA_PERIOD: usize = 500;
        let alpha = 2.0 / (EMA_PERIOD as f64 + 1.0);

        self.ema = Some(match self.ema {
            None => ratio,
            Some(prev) => prev + alpha * (ratio - prev),
        });
        self.ema_n += 1;

        if self.ema_n < EMA_PERIOD {
            return PairState::Flat;
        }

        let mean = self.ema.unwrap();
        let upper = mean * (1.0 + threshold_pct / 100.0);
        let lower = mean * (1.0 - threshold_pct / 100.0);

        if self.state != PairState::LongRatio && ratio > upper {
            self.state = PairState::LongRatio;
        } else if self.state != PairState::ShortRatio && ratio < lower {
            self.state = PairState::ShortRatio;
        }
        // no else: keep previous state (no re-emit)

        self.state
    }
}

// ---------------------------------------------------------------------------
// Per-symbol rolling price window (for correlation)
// ---------------------------------------------------------------------------

struct SymbolWindow {
    prices: VecDeque<Decimal>,
    symbol: Symbol,
}

impl SymbolWindow {
    fn new(symbol: Symbol, capacity: usize) -> Self {
        Self {
            prices: VecDeque::with_capacity(capacity),
            symbol,
        }
    }

    fn push(&mut self, price: Decimal, capacity: usize) {
        self.prices.push_back(price);
        if self.prices.len() > capacity {
            self.prices.pop_front();
        }
    }

    fn is_ready(&self, needed: usize) -> bool {
        self.prices.len() >= needed
    }

    /// Return log-returns for the price series (length = prices.len() - 1).
    fn log_returns(&self) -> Vec<f64> {
        let p: Vec<f64> = self.prices.iter().map(|d| d.to_f64_lossy()).collect();

        p.windows(2)
            .map(|w| if w[0] > 0.0 { (w[1] / w[0]).ln() } else { 0.0 })
            .collect()
    }
}

// Decimal helper (rust_decimal doesn't have to_f64_lossy in all versions)
trait ToF64 {
    fn to_f64_lossy(&self) -> f64;
}
impl ToF64 for Decimal {
    fn to_f64_lossy(&self) -> f64 {
        rust_decimal::prelude::ToPrimitive::to_f64(self).unwrap_or(0.0)
    }
}

// ---------------------------------------------------------------------------
// PearsonCorrelationPairsTradingAlphaModel
// ---------------------------------------------------------------------------

/// Pairs-trading alpha model that ranks every possible pair by Pearson correlation
/// of their log-return series and trades only the pair with the highest correlation
/// (if it exceeds `minimum_correlation`).
///
/// Mirrors C# `PearsonCorrelationPairsTradingAlphaModel`.
///
/// Signal logic (same as `BasePairsTradingAlphaModel` / `PairsTradingAlphaModel`):
/// - Compute ratio = price_a / price_b.
/// - Track an EMA-500 of the ratio as a rolling mean.
/// - If ratio > mean × (1 + threshold/100) → short asset_a, long  asset_b.
/// - If ratio < mean × (1 - threshold/100) → long  asset_a, short asset_b.
/// - Never re-emit the same regime.
pub struct PearsonCorrelationPairsTradingAlphaModel {
    lookback: usize,
    insight_period: TimeSpan,
    minimum_correlation: f64,
    /// Spread threshold as a percentage (0–100).
    threshold_pct: f64,
    /// Rolling price windows keyed by symbol SID.
    windows: HashMap<u64, SymbolWindow>,
    /// Active pair (if any).
    best_pair: Option<(u64, u64)>,
    /// Per-pair spread state keyed by (sid_a, sid_b).
    pairs: HashMap<(u64, u64), PairData>,
}

impl PearsonCorrelationPairsTradingAlphaModel {
    /// * `lookback`            – number of bars used for correlation calculation.
    /// * `insight_period`      – lifetime of emitted insights.
    /// * `threshold_pct`       – % deviation of ratio from EMA mean to trigger signal (C# default: 1.0).
    /// * `minimum_correlation` – minimum Pearson r to consider a pair tradable (C# default: 0.5).
    pub fn new(
        lookback: usize,
        insight_period: TimeSpan,
        threshold_pct: f64,
        minimum_correlation: f64,
    ) -> Self {
        Self {
            lookback,
            insight_period,
            minimum_correlation,
            threshold_pct,
            windows: HashMap::new(),
            best_pair: None,
            pairs: HashMap::new(),
        }
    }

    /// Convenience constructor matching C# defaults:
    ///   lookback=15 bars, threshold=1%, minimumCorrelation=0.5.
    /// `insight_period` = `lookback` calendar days.
    pub fn with_defaults(lookback: usize) -> Self {
        let period = TimeSpan::from_nanos(lookback as i64 * 86_400 * 1_000_000_000);
        Self::new(lookback, period, 1.0, 0.5)
    }

    /// Re-evaluate which pair has the highest Pearson correlation using the current
    /// rolling windows.  Called after every `on_securities_changed`.
    fn recompute_best_pair(&mut self) {
        let sids: Vec<u64> = self.windows.keys().copied().collect();

        let mut best_corr: f64 = f64::NEG_INFINITY;
        let mut best: Option<(u64, u64)> = None;

        for i in 0..sids.len() {
            for j in (i + 1)..sids.len() {
                let sid_a = sids[i];
                let sid_b = sids[j];

                let win_a = match self.windows.get(&sid_a) {
                    Some(w) if w.is_ready(self.lookback + 1) => w,
                    _ => continue,
                };
                let win_b = match self.windows.get(&sid_b) {
                    Some(w) if w.is_ready(self.lookback + 1) => w,
                    _ => continue,
                };

                let ret_a = win_a.log_returns();
                let ret_b = win_b.log_returns();

                // Both return series must have the same non-zero length.
                let len = ret_a.len().min(ret_b.len());
                if len < 2 {
                    continue;
                }

                if let Some(r) = pearson_correlation(&ret_a[..len], &ret_b[..len]) {
                    if r > best_corr {
                        best_corr = r;
                        best = Some((sid_a, sid_b));
                    }
                }
            }
        }

        if best_corr >= self.minimum_correlation {
            self.best_pair = best;
        } else {
            self.best_pair = None;
        }
    }
}

impl IAlphaModel for PearsonCorrelationPairsTradingAlphaModel {
    fn update(&mut self, slice: &Slice, _securities: &[Symbol]) -> Vec<Insight> {
        // Update rolling price windows for every subscribed symbol.
        for (sid, win) in self.windows.iter_mut() {
            if let Some(bar) = slice.bars.get(sid) {
                win.push(bar.close, self.lookback + 1);
            }
        }

        // No active pair → nothing to do.
        let (sid_a, sid_b) = match self.best_pair {
            Some(p) => p,
            None => return vec![],
        };

        // Need prices for both symbols.
        let price_a = match self
            .windows
            .get(&sid_a)
            .and_then(|w| w.prices.back().copied())
        {
            Some(p) => p,
            None => return vec![],
        };
        let price_b = match self
            .windows
            .get(&sid_b)
            .and_then(|w| w.prices.back().copied())
        {
            Some(p) => p,
            None => return vec![],
        };

        if price_b.is_zero() {
            return vec![];
        }

        let ratio = (price_a / price_b).to_f64_lossy();
        let pair_data = self
            .pairs
            .entry((sid_a, sid_b))
            .or_insert_with(PairData::new);

        let prev_state = pair_data.state;
        let new_state = pair_data.update(ratio, self.threshold_pct);

        // Emit only on state transitions.
        if new_state == prev_state || new_state == PairState::Flat {
            return vec![];
        }

        let sym_a = match self.windows.get(&sid_a) {
            Some(w) => w.symbol.clone(),
            None => return vec![],
        };
        let sym_b = match self.windows.get(&sid_b) {
            Some(w) => w.symbol.clone(),
            None => return vec![],
        };

        let (dir_a, dir_b) = match new_state {
            PairState::LongRatio => {
                // ratio > upper → asset_a expensive, asset_b cheap
                // short asset_a, long asset_b
                (InsightDirection::Down, InsightDirection::Up)
            }
            PairState::ShortRatio => {
                // ratio < lower → asset_a cheap, asset_b expensive
                // long asset_a, short asset_b
                (InsightDirection::Up, InsightDirection::Down)
            }
            PairState::Flat => return vec![],
        };

        vec![
            Insight::new(sym_a, dir_a, self.insight_period, None, None, self.name()),
            Insight::new(sym_b, dir_b, self.insight_period, None, None, self.name()),
        ]
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for s in added {
            self.windows
                .entry(s.id.sid)
                .or_insert_with(|| SymbolWindow::new(s.clone(), self.lookback + 1));
        }
        for s in removed {
            self.windows.remove(&s.id.sid);
            // Remove all pairs that involve this symbol.
            let sid = s.id.sid;
            self.pairs.retain(|&(a, b), _| a != sid && b != sid);
            if let Some((a, b)) = self.best_pair {
                if a == sid || b == sid {
                    self.best_pair = None;
                }
            }
        }

        // Recompute the best pair whenever the universe changes.
        self.recompute_best_pair();
    }

    fn name(&self) -> &str {
        "PearsonCorrelationPairsTradingAlphaModel"
    }
}
