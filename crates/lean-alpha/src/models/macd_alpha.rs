use std::collections::HashMap;

use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use lean_indicators::{Indicator, Macd};

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

struct MacdState {
    macd: Macd,
    prev_direction: Option<InsightDirection>,
    symbol: Symbol,
}

/// MACD crossover alpha model.
/// When the MACD line crosses above the signal line → Up.
/// When it crosses below → Down.
pub struct MacdAlphaModel {
    fast_period: usize,
    slow_period: usize,
    signal_period: usize,
    insight_period: TimeSpan,
    state: HashMap<u64, MacdState>,
}

impl MacdAlphaModel {
    pub fn new(
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        insight_period: TimeSpan,
    ) -> Self {
        Self {
            fast_period,
            slow_period,
            signal_period,
            insight_period,
            state: HashMap::new(),
        }
    }
}

impl IAlphaModel for MacdAlphaModel {
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

            state.macd.update_bar(&bar);

            if !state.macd.is_ready() {
                continue;
            }

            // macd_line > signal_line → bullish; < → bearish
            let direction = if state.macd.macd_line > state.macd.signal_line {
                InsightDirection::Up
            } else if state.macd.macd_line < state.macd.signal_line {
                InsightDirection::Down
            } else {
                InsightDirection::Flat
            };

            // Emit only on crossover (direction change)
            if Some(direction) != state.prev_direction {
                state.prev_direction = Some(direction);
                insights.push(Insight::new(
                    state.symbol.clone(),
                    direction,
                    self.insight_period,
                    None,
                    None,
                    "MacdAlphaModel",
                ));
            }
        }

        insights
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for s in added {
            self.state.entry(s.id.sid).or_insert_with(|| MacdState {
                macd: Macd::new(self.fast_period, self.slow_period, self.signal_period),
                prev_direction: None,
                symbol: s.clone(),
            });
        }
        for s in removed {
            self.state.remove(&s.id.sid);
        }
    }

    fn name(&self) -> &str {
        "MacdAlphaModel"
    }
}
