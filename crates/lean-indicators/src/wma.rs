use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Linear Weighted Moving Average (WMA). Weight = position (n, n-1, ..., 1).
pub struct Wma {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    denominator: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Wma {
    pub fn new(period: usize) -> Self {
        let n = Decimal::from(period);
        let denom = n * (n + dec!(1)) / dec!(2);
        Wma {
            name: format!("WMA({})", period),
            period,
            window: RollingWindow::new(period),
            denominator: denom,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn compute(&self) -> Price {
        let n = self.window.len();
        let mut numerator = dec!(0);
        // window[0] = newest, weight = period; window[n-1] = oldest, weight = 1
        for i in 0..n {
            let weight = Decimal::from(n - i);
            numerator += weight * self.window.get(i).copied().unwrap_or(dec!(0));
        }
        if self.denominator == dec!(0) {
            return dec!(0);
        }
        numerator / self.denominator
    }
}

impl Indicator for Wma {
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
        self.window.push(value);
        self.samples += 1;

        if self.is_ready() {
            let v = self.compute();
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
