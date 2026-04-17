use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal_macros::dec;

/// MidPrice = (High + Low) / 2 for each bar.
pub struct MidPrice {
    name: String,
    samples: usize,
    current: IndicatorResult,
}

impl MidPrice {
    pub fn new() -> Self {
        MidPrice {
            name: "MIDPRICE".to_string(),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Default for MidPrice {
    fn default() -> Self {
        Self::new()
    }
}

impl Indicator for MidPrice {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= 1
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let mid = (bar.high + bar.low) / dec!(2);
        self.current = IndicatorResult::ready(mid, bar.time);
        self.current.clone()
    }
}
