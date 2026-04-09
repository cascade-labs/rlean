use std::collections::HashMap;

use lean_core::{Symbol, TimeSpan};
use lean_data::Slice;
use lean_indicators::{Indicator, Rsi};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::{
    alpha_model::IAlphaModel,
    insight::{Insight, InsightDirection},
};

/// RSI-based alpha model.
/// Overbought (>= 70) → Down; Oversold (<= 30) → Up; else Flat.
pub struct RsiAlphaModel {
    period: usize,
    overbought: Decimal,
    oversold: Decimal,
    insight_period: TimeSpan,
    state: HashMap<u64, RsiState>,
}

struct RsiState {
    rsi: Rsi,
    prev_direction: Option<InsightDirection>,
    symbol: Symbol,
}

impl RsiAlphaModel {
    pub fn new(period: usize, insight_period: TimeSpan) -> Self {
        Self {
            period,
            overbought: dec!(70),
            oversold: dec!(30),
            insight_period,
            state: HashMap::new(),
        }
    }

    pub fn with_thresholds(
        period: usize,
        overbought: Decimal,
        oversold: Decimal,
        insight_period: TimeSpan,
    ) -> Self {
        Self {
            period,
            overbought,
            oversold,
            insight_period,
            state: HashMap::new(),
        }
    }
}

impl IAlphaModel for RsiAlphaModel {
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

            state.rsi.update_bar(&bar);

            if !state.rsi.is_ready() {
                continue;
            }

            let rsi_val = state.rsi.current().value;

            let direction = if rsi_val >= self.overbought {
                InsightDirection::Down
            } else if rsi_val <= self.oversold {
                InsightDirection::Up
            } else {
                InsightDirection::Flat
            };

            if Some(direction) != state.prev_direction {
                state.prev_direction = Some(direction);
                insights.push(Insight::new(
                    state.symbol.clone(),
                    direction,
                    self.insight_period,
                    None,
                    None,
                    "RsiAlphaModel",
                ));
            }
        }

        insights
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for s in added {
            self.state.entry(s.id.sid).or_insert_with(|| RsiState {
                rsi: Rsi::new(self.period),
                prev_direction: None,
                symbol: s.clone(),
            });
        }
        for s in removed {
            self.state.remove(&s.id.sid);
        }
    }

    fn name(&self) -> &str {
        "RsiAlphaModel"
    }
}
