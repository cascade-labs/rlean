use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

/// Log Return. ln(close / prev_close).
pub struct LogReturn {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl LogReturn {
    pub fn new(period: usize) -> Self {
        LogReturn {
            name: format!("LOGR({})", period),
            period,
            window: RollingWindow::new(period + 1),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for LogReturn {
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
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.window.push(value);

        if self.window.len() >= 2 {
            let newest = self.window.newest().and_then(|v| v.to_f64()).unwrap_or(0.0);
            let oldest = self.window.oldest().and_then(|v| v.to_f64()).unwrap_or(0.0);

            if oldest != 0.0 {
                let ratio = newest / oldest;
                let ln_val = ratio.ln();
                if ln_val.is_finite() {
                    let v = rust_decimal::Decimal::from_f64_retain(ln_val).unwrap_or(dec!(0));
                    if self.is_ready() {
                        self.current = IndicatorResult::ready(v, time);
                    }
                }
            }
        }

        self.current.clone()
    }
}
