use crate::coarse_fundamental::CoarseFundamental;
use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

/// EMA state for a single symbol.
/// Tracks a fast and a slow exponential moving average.
struct EmaState {
    fast_period: usize,
    slow_period: usize,
    fast_value: Option<Decimal>,
    slow_value: Option<Decimal>,
    fast_count: usize,
    slow_count: usize,
}

impl EmaState {
    fn new(fast_period: usize, slow_period: usize) -> Self {
        Self {
            fast_period,
            slow_period,
            fast_value: None,
            slow_value: None,
            fast_count: 0,
            slow_count: 0,
        }
    }

    /// Update with a new price sample.
    /// Returns true when both EMAs are ready (warm).
    fn update(&mut self, price: Decimal) -> bool {
        let fast_k = Decimal::from(2) / (Decimal::from(self.fast_period + 1));
        let slow_k = Decimal::from(2) / (Decimal::from(self.slow_period + 1));

        match self.fast_value {
            None => {
                self.fast_value = Some(price);
                self.fast_count = 1;
            }
            Some(prev) => {
                let next = price * fast_k + prev * (Decimal::ONE - fast_k);
                self.fast_value = Some(next);
                if self.fast_count < self.fast_period {
                    self.fast_count += 1;
                }
            }
        }

        match self.slow_value {
            None => {
                self.slow_value = Some(price);
                self.slow_count = 1;
            }
            Some(prev) => {
                let next = price * slow_k + prev * (Decimal::ONE - slow_k);
                self.slow_value = Some(next);
                if self.slow_count < self.slow_period {
                    self.slow_count += 1;
                }
            }
        }

        self.fast_count >= self.fast_period && self.slow_count >= self.slow_period
    }

    /// Returns the scaled delta: (fast - slow) / ((fast + slow) / 2)
    /// Only valid when both EMAs are ready.
    fn scaled_delta(&self) -> Option<Decimal> {
        let fast = self.fast_value?;
        let slow = self.slow_value?;
        let midpoint = (fast + slow) / Decimal::from(2);
        if midpoint.is_zero() {
            return None;
        }
        Some((fast - slow) / midpoint)
    }

    fn fast(&self) -> Option<Decimal> {
        self.fast_value
    }

    fn slow(&self) -> Option<Decimal> {
        self.slow_value
    }
}

/// Tolerance for fast > slow comparison (mirrors C# 0.01m).
const TOLERANCE: &str = "0.01";

/// Selects symbols where fast EMA > slow EMA * (1 + tolerance),
/// ranked by scaled delta (largest delta first), up to `universe_count`.
///
/// Mirrors C# `EmaCrossUniverseSelectionModel`.
pub struct EmaCrossUniverseSelectionModel {
    fast_period: usize,
    slow_period: usize,
    universe_count: usize,
    min_dollar_volume: Option<Decimal>,
    averages: HashMap<String, EmaState>,
}

impl EmaCrossUniverseSelectionModel {
    /// Create a new model.
    ///
    /// * `fast_period`      – periods for the fast EMA (default 100)
    /// * `slow_period`      – periods for the slow EMA (default 300)
    /// * `universe_count`   – max symbols to return (default 500)
    /// * `min_dollar_volume` – optional coarse pre-filter threshold
    pub fn new(fast_period: usize, slow_period: usize, universe_count: usize) -> Self {
        Self {
            fast_period,
            slow_period,
            universe_count,
            min_dollar_volume: None,
            averages: HashMap::new(),
        }
    }

    /// Fluent builder: add a minimum dollar-volume pre-filter.
    pub fn with_min_dollar_volume(mut self, threshold: Decimal) -> Self {
        self.min_dollar_volume = Some(threshold);
        self
    }

    /// Feed one day of coarse data and return the selected symbols.
    ///
    /// Internally updates per-symbol EMA state, then selects the top
    /// `universe_count` symbols where fast > slow * (1 + tolerance).
    pub fn select(&mut self, coarse: &[CoarseFundamental]) -> Vec<Symbol> {
        let tolerance: Decimal = TOLERANCE.parse().unwrap();

        // Apply optional coarse pre-filter.
        let filtered: Vec<&CoarseFundamental> = match self.min_dollar_volume {
            Some(min_dv) => coarse
                .iter()
                .filter(|c| c.dollar_volume >= min_dv)
                .collect(),
            None => coarse.iter().collect(),
        };

        // Update EMA state for each coarse entry; collect ready symbols.
        let mut candidates: Vec<(Decimal, Symbol)> = filtered
            .into_iter()
            .filter_map(|cf| {
                let state = self
                    .averages
                    .entry(cf.symbol.value.clone())
                    .or_insert_with(|| EmaState::new(self.fast_period, self.slow_period));

                let ready = state.update(cf.price);
                if !ready {
                    return None;
                }

                let fast = state.fast()?;
                let slow = state.slow()?;

                // Only include bullish crosses: fast > slow * (1 + tolerance)
                if fast <= slow * (Decimal::ONE + tolerance) {
                    return None;
                }

                let delta = state.scaled_delta()?;
                Some((delta, cf.symbol.clone()))
            })
            .collect();

        // Sort descending by scaled delta.
        candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(self.universe_count);
        candidates.into_iter().map(|(_, sym)| sym).collect()
    }
}

impl Default for EmaCrossUniverseSelectionModel {
    fn default() -> Self {
        Self::new(100, 300, 500)
    }
}
