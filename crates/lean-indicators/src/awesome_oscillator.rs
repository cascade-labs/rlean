use crate::{indicator::{Indicator, IndicatorResult}, sma::Sma};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal_macros::dec;

/// Awesome Oscillator. SMA(midpoint,5) - SMA(midpoint,34).
pub struct AwesomeOscillator {
    name: String,
    fast: Sma,
    slow: Sma,
    samples: usize,
    current: IndicatorResult,
}

impl AwesomeOscillator {
    pub fn new(fast_period: usize, slow_period: usize) -> Self {
        AwesomeOscillator {
            name: format!("AO({},{})", fast_period, slow_period),
            fast: Sma::new(fast_period),
            slow: Sma::new(slow_period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default() -> Self {
        Self::new(5, 34)
    }
}

impl Indicator for AwesomeOscillator {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.fast.is_ready() && self.slow.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.slow.warm_up_period() }

    fn reset(&mut self) {
        self.fast.reset();
        self.slow.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let mid = (bar.high + bar.low) / dec!(2);
        let rf = self.fast.update_price(bar.time, mid);
        let rs = self.slow.update_price(bar.time, mid);

        if rf.is_ready() && rs.is_ready() {
            self.current = IndicatorResult::ready(rf.value - rs.value, bar.time);
        }

        self.current.clone()
    }
}
