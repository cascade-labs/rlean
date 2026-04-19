use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal_macros::dec;

/// Balance of Power. (Close - Open) / (High - Low).
pub struct BalanceOfPower {
    name: String,
    samples: usize,
    current: IndicatorResult,
}

impl BalanceOfPower {
    pub fn new() -> Self {
        BalanceOfPower {
            name: "BOP".to_string(),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Default for BalanceOfPower {
    fn default() -> Self {
        Self::new()
    }
}

impl Indicator for BalanceOfPower {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > 0
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
        let range = bar.high - bar.low;
        let v = if range > dec!(0) {
            (bar.close - bar.open) / range
        } else {
            dec!(0)
        };
        self.current = IndicatorResult::ready(v, bar.time);
        self.current.clone()
    }
}
