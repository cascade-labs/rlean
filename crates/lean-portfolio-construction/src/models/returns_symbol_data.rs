//! Shared per-symbol returns history for optimization-based portfolio models.
//!
//! Mirrors C# LEAN `ReturnsSymbolData.cs`: a `RateOfChange` indicator feeds a
//! `RollingWindow` of already-computed returns, so the returns are calculated
//! incrementally inside the indicator rather than recomputed from a raw price
//! buffer on every rebalance.
//!
//! Both `BlackLittermanOptimizationPortfolioConstructionModel` and
//! `RiskParityPortfolioConstructionModel` use this so the rolling-returns logic
//! lives in one place and reuses `lean-indicators`.

use std::collections::HashMap;

use lean_core::DateTime;
use lean_indicators::{roc::Roc, window::RollingWindow, Indicator};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Returns specific to a single symbol required by optimization models.
///
/// The `RateOfChange` indicator computes `lookback`-step returns; ready values
/// are pushed into a rolling window holding the most recent `period` returns.
pub struct ReturnsSymbolData {
    roc: Roc,
    /// Rolling window of computed returns (raw ratio, e.g. 0.01 == +1%),
    /// newest at index 0 to match LEAN's `RollingWindow` semantics.
    window: RollingWindow<f64>,
}

impl ReturnsSymbolData {
    /// * `lookback` — look-back period for the rate-of-change indicator.
    /// * `period` — size of the rolling window of historical returns.
    pub fn new(lookback: usize, period: usize) -> Self {
        Self {
            roc: Roc::new(lookback),
            window: RollingWindow::new(period.max(1)),
        }
    }

    /// Update the rate-of-change with a new price; pushes the computed return
    /// into the rolling window once the indicator is ready.
    pub fn update(&mut self, time: DateTime, price: f64) {
        let value = match Decimal::try_from(price) {
            Ok(v) => v,
            Err(_) => return,
        };
        let result = self.roc.update_price(time, value);
        if result.is_ready() {
            // `Roc` is expressed in percent (×100); divide back to a raw ratio
            // to match LEAN's `RateOfChange` and downstream annualisation.
            let ret = result.value.to_f64().unwrap_or(0.0) / 100.0;
            self.window.push(ret);
        }
    }

    /// Historical returns, oldest-first (chronological order).
    pub fn returns(&self) -> Vec<f64> {
        // `RollingWindow` is newest-first; reverse to chronological order.
        let mut rets: Vec<f64> = self.window.iter().copied().collect();
        rets.reverse();
        rets
    }

    /// Number of returns currently stored.
    pub fn len(&self) -> usize {
        self.window.len()
    }

    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }
}

/// Build a returns matrix (rows = time, cols = asset) for the ordered symbol id
/// list from a map of per-symbol returns data.
///
/// Returns `None` if no asset has any returns. Rows are truncated to the
/// shortest available history so every column is aligned, matching the prior
/// behaviour of the per-model `build_returns_matrix` helpers.
pub fn form_returns_matrix(
    asset_data: &HashMap<u64, ReturnsSymbolData>,
    symbols: &[u64],
) -> Option<Vec<Vec<f64>>> {
    let per_asset: Vec<Vec<f64>> = symbols
        .iter()
        .map(|t| asset_data.get(t).map(|d| d.returns()).unwrap_or_default())
        .collect();

    let n_rows = per_asset.iter().map(|v| v.len()).min().unwrap_or(0);
    if n_rows == 0 {
        return None;
    }

    let n_cols = symbols.len();
    let matrix: Vec<Vec<f64>> = (0..n_rows)
        .map(|t| (0..n_cols).map(|c| per_asset[c][t]).collect())
        .collect();

    Some(matrix)
}
