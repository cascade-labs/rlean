use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

/// Least Squares Moving Average. Linear regression over n bars, value at current bar.
pub struct Lsma {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Lsma {
    pub fn new(period: usize) -> Self {
        Lsma {
            name: format!("LSMA({})", period),
            period,
            window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    fn compute(&self) -> Decimal {
        let n = self.period as f64;
        let mut sum_x = 0.0f64;
        let mut sum_y = 0.0f64;
        let mut sum_xy = 0.0f64;
        let mut sum_x2 = 0.0f64;

        // window[0] = newest = x=period, window[period-1] = oldest = x=1
        for i in 0..self.period {
            let x = (self.period - i) as f64; // newest has highest x
            let y = self.window.get(i).and_then(|v| v.to_f64()).unwrap_or(0.0);
            sum_x += x;
            sum_y += y;
            sum_xy += x * y;
            sum_x2 += x * x;
        }

        let denom = n * sum_x2 - sum_x * sum_x;
        if denom == 0.0 { return dec!(0); }
        let b = (n * sum_xy - sum_x * sum_y) / denom;
        let a = (sum_y - b * sum_x) / n;
        // value at current bar = a + b * period
        let result = a + b * n;
        Decimal::from_f64_retain(result).unwrap_or(dec!(0))
    }
}

impl Indicator for Lsma {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.window.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.window.push(value);

        if self.is_ready() {
            let v = self.compute();
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
