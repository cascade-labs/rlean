use std::collections::HashMap;

use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use lean_indicators::{Ema, Indicator};

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

struct EmaCrossState {
    fast: Ema,
    slow: Ema,
    prev_direction: Option<InsightDirection>,
}

/// Emits Up when fast EMA crosses above slow EMA, Down on the reverse.
pub struct EmaCrossAlphaModel {
    fast_period: usize,
    slow_period: usize,
    insight_period: TimeSpan,
    state: HashMap<u64, EmaCrossState>,
}

impl EmaCrossAlphaModel {
    pub fn new(fast_period: usize, slow_period: usize, insight_period: TimeSpan) -> Self {
        Self {
            fast_period,
            slow_period,
            insight_period,
            state: HashMap::new(),
        }
    }
}

impl IAlphaModel for EmaCrossAlphaModel {
    fn update(&mut self, slice: &Slice, _securities: &[Symbol]) -> Vec<Insight> {
        let mut insights = Vec::new();

        // Collect symbols whose state we need to update (borrow issues workaround)
        let sids: Vec<u64> = self.state.keys().copied().collect();

        for sid in sids {
            // Find the corresponding bar in the slice
            let bar = match slice.bars.get(&sid) {
                Some(b) => b.clone(),
                None => continue,
            };

            let state = match self.state.get_mut(&sid) {
                Some(s) => s,
                None => continue,
            };

            state.fast.update_bar(&bar);
            state.slow.update_bar(&bar);

            if !state.fast.is_ready() || !state.slow.is_ready() {
                continue;
            }

            let fast_val = state.fast.current().value;
            let slow_val = state.slow.current().value;

            let direction = if fast_val > slow_val {
                InsightDirection::Up
            } else if fast_val < slow_val {
                InsightDirection::Down
            } else {
                InsightDirection::Flat
            };

            // Only emit an insight when direction changes (crossover event)
            if Some(direction) != state.prev_direction {
                state.prev_direction = Some(direction);
                insights.push(Insight::new(
                    bar.symbol.clone(),
                    direction,
                    self.insight_period,
                    None,
                    None,
                    self.name(),
                ));
            }
        }

        insights
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for s in added {
            self.state.entry(s.id.sid).or_insert_with(|| EmaCrossState {
                fast: Ema::new(self.fast_period),
                slow: Ema::new(self.slow_period),
                prev_direction: None,
            });
        }
        for s in removed {
            self.state.remove(&s.id.sid);
        }
    }

    fn name(&self) -> &str {
        "EmaCrossAlphaModel"
    }
}
