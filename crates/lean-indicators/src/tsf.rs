use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

/// Time Series Forecast. Linear regression projected 1 period ahead.
pub struct Tsf {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Tsf {
    pub fn new(period: usize) -> Self {
        Tsf {
            name: format!("TSF({})", period),
            period,
            window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Tsf {
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

        if !self.is_ready() {
            return self.current.clone();
        }

        // From tulipindicators TSF algorithm
        // window[0] = newest, window[period-1] = oldest
        // x goes from 1 (oldest) to period (newest)
        let p = self.period as f64;
        let mut x1 = 0.0f64;
        let mut x2 = 0.0f64;
        let mut xy = 0.0f64;
        let mut y = 0.0f64;

        // i = 0 is newest (x = period), i = period-1 is oldest (x = 1)
        for i in 0..self.period {
            let x = (self.period - i) as f64;
            let val = self.window.get(i).and_then(|v| v.to_f64()).unwrap_or(0.0);
            x1 += x;
            x2 += x * x;
            xy += val * x;
            y += val;
        }

        let bd = 1.0 / (p * x2 - x1 * x1);
        let b = (p * xy - x1 * y) * bd;
        let a = (y - b * x1) / p;

        // forecast 1 period ahead = a + b*(period+1)
        let result = a + b * (p + 1.0);
        let v = Decimal::from_f64_retain(result).unwrap_or(dec!(0));
        self.current = IndicatorResult::ready(v, time);
        self.current.clone()
    }
}
