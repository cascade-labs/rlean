use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Simple Moving Average.
pub struct Sma {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    running_sum: Price,
    samples: usize,
    current: IndicatorResult,
}

impl Sma {
    pub fn new(period: usize) -> Self {
        Sma {
            name: format!("SMA({})", period),
            period,
            window: RollingWindow::new(period),
            running_sum: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Sma {
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
        self.period
    }

    fn reset(&mut self) {
        self.window.clear();
        self.running_sum = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        // Remove outgoing value from sum
        if self.window.is_full() {
            if let Some(oldest) = self.window.oldest() {
                self.running_sum -= *oldest;
            }
        }
        self.window.push(value);
        self.running_sum += value;
        self.samples += 1;

        if self.is_ready() {
            let avg = self.running_sum / Decimal::from(self.period);
            self.current = IndicatorResult::ready(avg, time);
        }

        self.current.clone()
    }
}
