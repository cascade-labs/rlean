use std::collections::HashMap;

use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use rust_decimal::Decimal;

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

struct MomentumState {
    /// Closing prices, most-recent last.
    prices: Vec<Decimal>,
    symbol: Symbol,
    prev_direction: Option<InsightDirection>,
}

/// N-bar momentum alpha model.
/// Computes return = (close_now - close_N_bars_ago) / close_N_bars_ago.
/// If return > threshold → Up; return < -threshold → Down; else Flat.
pub struct MomentumAlphaModel {
    lookback: usize,
    threshold: Decimal,
    insight_period: TimeSpan,
    state: HashMap<u64, MomentumState>,
}

impl MomentumAlphaModel {
    pub fn new(lookback: usize, threshold: Decimal, insight_period: TimeSpan) -> Self {
        Self {
            lookback,
            threshold,
            insight_period,
            state: HashMap::new(),
        }
    }
}

impl IAlphaModel for MomentumAlphaModel {
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
            // Keep only lookback + 1 prices
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

            let ret = (bar.close - oldest) / oldest;

            let direction = if ret > self.threshold {
                InsightDirection::Up
            } else if ret < -self.threshold {
                InsightDirection::Down
            } else {
                InsightDirection::Flat
            };

            if Some(direction) != state.prev_direction {
                state.prev_direction = Some(direction);
                insights.push(Insight::new(
                    state.symbol.clone(),
                    direction,
                    self.insight_period,
                    Some(ret.abs()),
                    None,
                    "MomentumAlphaModel",
                ));
            }
        }

        insights
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for s in added {
            self.state.entry(s.id.sid).or_insert_with(|| MomentumState {
                prices: Vec::with_capacity(self.lookback + 1),
                symbol: s.clone(),
                prev_direction: None,
            });
        }
        for s in removed {
            self.state.remove(&s.id.sid);
        }
    }

    fn name(&self) -> &str {
        "MomentumAlphaModel"
    }
}
