use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

/// Pairs-trading alpha model based on the z-score of the price spread.
///
/// When z-score > `entry_z` → symbol_a is expensive, symbol_b is cheap →
///   emit Down for symbol_a, Up for symbol_b (mean-revert).
/// When z-score < -`entry_z` → opposite.
pub struct PairsTradingAlphaModel {
    symbol_a: Symbol,
    symbol_b: Symbol,
    insight_period: TimeSpan,
    entry_z: Decimal,
    /// Rolling spread history for z-score calculation.
    spreads: Vec<Decimal>,
    lookback: usize,
    prev_direction_a: Option<InsightDirection>,
}

impl PairsTradingAlphaModel {
    /// `lookback` — number of bars used to estimate mean and std of spread.
    pub fn new(
        symbol_a: Symbol,
        symbol_b: Symbol,
        lookback: usize,
        entry_z: Decimal,
        insight_period: TimeSpan,
    ) -> Self {
        Self {
            symbol_a,
            symbol_b,
            insight_period,
            entry_z,
            spreads: Vec::with_capacity(lookback),
            lookback,
            prev_direction_a: None,
        }
    }

    fn z_score(&self, spread: Decimal) -> Option<Decimal> {
        let n = self.spreads.len();
        if n < 2 {
            return None;
        }

        let n_dec = Decimal::from(n as i64);
        let mean: Decimal = self.spreads.iter().copied().sum::<Decimal>() / n_dec;

        // Population variance (fast, no allocation)
        let var: Decimal = self
            .spreads
            .iter()
            .map(|&s| {
                let d = s - mean;
                d * d
            })
            .sum::<Decimal>()
            / n_dec;

        if var.is_zero() {
            return None;
        }

        // Decimal sqrt via Newton-Raphson (a few iterations are sufficient)
        let std = decimal_sqrt(var)?;
        Some((spread - mean) / std)
    }
}

impl IAlphaModel for PairsTradingAlphaModel {
    fn update(&mut self, slice: &Slice, _securities: &[Symbol]) -> Vec<Insight> {
        let bar_a = match slice.get_bar(&self.symbol_a) {
            Some(b) => b.clone(),
            None => return vec![],
        };
        let bar_b = match slice.get_bar(&self.symbol_b) {
            Some(b) => b.clone(),
            None => return vec![],
        };

        let spread = bar_a.close - bar_b.close;

        // Update rolling spread history
        self.spreads.push(spread);
        if self.spreads.len() > self.lookback {
            self.spreads.remove(0);
        }

        let z = match self.z_score(spread) {
            Some(z) => z,
            None => return vec![],
        };

        let direction_a = if z > self.entry_z {
            InsightDirection::Down
        } else if z < -self.entry_z {
            InsightDirection::Up
        } else {
            InsightDirection::Flat
        };

        // Only emit on regime change
        if Some(direction_a) == self.prev_direction_a {
            return vec![];
        }
        self.prev_direction_a = Some(direction_a);

        let direction_b = match direction_a {
            InsightDirection::Up => InsightDirection::Down,
            InsightDirection::Down => InsightDirection::Up,
            InsightDirection::Flat => InsightDirection::Flat,
        };

        vec![
            Insight::new(
                self.symbol_a.clone(),
                direction_a,
                self.insight_period,
                None,
                None,
                "PairsTradingAlphaModel",
            ),
            Insight::new(
                self.symbol_b.clone(),
                direction_b,
                self.insight_period,
                None,
                None,
                "PairsTradingAlphaModel",
            ),
        ]
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {
        // Pairs model manages its own fixed pair; no action needed.
    }

    fn name(&self) -> &str {
        "PairsTradingAlphaModel"
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Integer-precision square root of a Decimal via Newton-Raphson.
/// Returns None if the value is negative or if convergence fails.
fn decimal_sqrt(val: Decimal) -> Option<Decimal> {
    if val < dec!(0) {
        return None;
    }
    if val.is_zero() {
        return Some(dec!(0));
    }

    let two = dec!(2);
    let mut x = val / two; // initial guess

    for _ in 0..50 {
        let next = (x + val / x) / two;
        let delta = (next - x).abs();
        x = next;
        // Converged when change is negligible
        if delta < dec!(0.000000001) {
            break;
        }
    }
    Some(x)
}
