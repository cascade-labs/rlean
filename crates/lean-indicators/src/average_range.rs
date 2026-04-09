use crate::{indicator::{Indicator, IndicatorResult}, sma::Sma};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal_macros::dec;

/// Average Range = SMA(High - Low, period).
pub struct AverageRange {
    name: String,
    sma: Sma,
    samples: usize,
    current: IndicatorResult,
}

impl AverageRange {
    pub fn new(period: usize) -> Self {
        AverageRange {
            name: format!("AVGRANGE({})", period),
            sma: Sma::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for AverageRange {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.sma.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.sma.warm_up_period() }

    fn reset(&mut self) {
        self.sma.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let range = bar.high - bar.low;
        let r = self.sma.update_price(bar.time, range);
        if r.is_ready() {
            self.current = IndicatorResult::ready(r.value, bar.time);
        }
        self.current.clone()
    }
}
