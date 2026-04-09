use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Momentum. close - close[n].
pub struct Momentum {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Momentum {
    pub fn new(period: usize) -> Self {
        Momentum {
            name: format!("MOM({})", period),
            period,
            window: RollingWindow::new(period + 1),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Momentum {
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
            let v = value - oldest;
            self.current = IndicatorResult::ready(v, time);
        } else if self.window.len() > 1 {
            // partial: delta from first item
            let oldest = self.window.oldest().copied().unwrap_or(dec!(0));
            let v = value - oldest;
            // not ready but compute anyway (matches LEAN behavior)
            let _ = v;
        }

        self.current.clone()
    }
}
