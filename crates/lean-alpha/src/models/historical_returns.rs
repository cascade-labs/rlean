use std::collections::HashMap;

use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use rust_decimal::Decimal;

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

struct HistoricalReturnsState {
    /// Rolling close prices, most-recent last.  Capacity = lookback + 1.
    prices: Vec<Decimal>,
    symbol: Symbol,
}

/// Alpha model that uses N-period historical returns to generate directional signals.
///
/// Mirrors C# `HistoricalReturnsAlphaModel`.
///
/// On each bar the rate-of-change over `lookback` periods is computed:
///   ROC = (close_now - close_lookback_ago) / close_lookback_ago
///
/// Insight direction:
///   ROC > 0 → Up   (magnitude = ROC)
///   ROC < 0 → Down (magnitude = |ROC|)
///   ROC = 0 → Flat (no insight emitted — mirrors C# cancel behaviour)
///
/// `insight_period` is the lifetime of each emitted insight.  In the C# engine it
/// equals `resolution * lookback`; the caller should pass that value directly.
/// A convenience constructor `with_daily_period` multiplies `lookback` days for you.
pub struct HistoricalReturnsAlphaModel {
    lookback: usize,
    insight_period: TimeSpan,
    state: HashMap<u64, HistoricalReturnsState>,
}

impl HistoricalReturnsAlphaModel {
    /// General constructor.
    ///
    /// * `lookback`       – number of bars used to compute the return.
    /// * `insight_period` – how long each emitted insight is valid.
    pub fn new(lookback: usize, insight_period: TimeSpan) -> Self {
        Self {
            lookback,
            insight_period,
            state: HashMap::new(),
        }
    }

    /// Convenience constructor for daily-resolution strategies.
    /// `insight_period` = `lookback` calendar days, matching the C# default.
    pub fn with_daily_period(lookback: usize) -> Self {
        let period = TimeSpan::from_nanos(lookback as i64 * 86_400 * 1_000_000_000);
        Self::new(lookback, period)
    }
}

impl IAlphaModel for HistoricalReturnsAlphaModel {
    fn update(&mut self, slice: &Slice, _securities: &[Symbol]) -> Vec<Insight> {
        let mut insights = Vec::new();
        let sids: Vec<u64> = self.state.keys().copied().collect();

        for sid in sids {
            let bar = match slice.bars.get(&sid) {
                Some(b) => b.clone(),
                None => continue,
            };

            let state = match self.state.get_mut(&sid) {
                Some(s) => s,
                None => continue,
            };

            state.prices.push(bar.close);
            // Keep lookback + 1 prices (current + lookback reference)
            if state.prices.len() > self.lookback + 1 {
                state.prices.remove(0);
            }

            if state.prices.len() < self.lookback + 1 {
                continue;
            }

            let oldest = state.prices[0];
            if oldest.is_zero() {
                continue;
            }

            let roc = (bar.close - oldest) / oldest;

            let direction = if roc > Decimal::ZERO {
                InsightDirection::Up
            } else if roc < Decimal::ZERO {
                InsightDirection::Down
            } else {
                // Flat — C# cancels existing insights; here we simply skip emission.
                continue;
            };

            insights.push(Insight::new(
                state.symbol.clone(),
                direction,
                self.insight_period,
                Some(roc.abs()),
                None,
                self.name(),
            ));
        }

        insights
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for s in added {
            self.state
                .entry(s.id.sid)
                .or_insert_with(|| HistoricalReturnsState {
                    prices: Vec::with_capacity(self.lookback + 1),
                    symbol: s.clone(),
                });
        }
        for s in removed {
            self.state.remove(&s.id.sid);
        }
    }

    fn name(&self) -> &str {
        "HistoricalReturnsAlphaModel"
    }
}
