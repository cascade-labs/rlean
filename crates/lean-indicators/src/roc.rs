use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Rate of Change = (close - close[n]) / close[n] * 100.
pub struct Roc {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Roc {
    pub fn new(period: usize) -> Self {
        Roc {
            name: format!("ROC({})", period),
            period,
            window: RollingWindow::new(period + 1),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Roc {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.window.is_full()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.period + 1
    }

    fn reset(&mut self) {
        self.window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.window.push(value);
        self.samples += 1;

        if self.window.is_full() {
            let old = self.window.oldest().copied().unwrap_or(dec!(1));
            let roc = if old.is_zero() {
                dec!(0)
            } else {
                (value - old) / old * dec!(100)
            };
            self.current = IndicatorResult::ready(roc, time);
        }

        self.current.clone()
    }
}
