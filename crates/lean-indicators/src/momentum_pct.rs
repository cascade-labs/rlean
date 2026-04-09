use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Momentum Percent. (close - close[n]) / close[n] * 100.
pub struct MomentumPct {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl MomentumPct {
    pub fn new(period: usize) -> Self {
        MomentumPct {
            name: format!("MOMP({})", period),
            period,
            window: RollingWindow::new(period + 1),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for MomentumPct {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples > self.period }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period + 1 }

    fn reset(&mut self) {
        self.window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.window.push(value);

        if self.is_ready() {
            let oldest = self.window.oldest().copied().unwrap_or(dec!(0));
            let v = if oldest != dec!(0) {
                (value - oldest) / oldest * dec!(100)
            } else {
                dec!(0)
            };
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
