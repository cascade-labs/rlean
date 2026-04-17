use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Rolling covariance between two series.
pub struct Covariance {
    name: String,
    period: usize,
    a_window: RollingWindow<Decimal>,
    b_window: RollingWindow<Decimal>,
    samples: usize,
    current: IndicatorResult,
}

impl Covariance {
    pub fn new(period: usize) -> Self {
        Covariance {
            name: format!("COV({})", period),
            period,
            a_window: RollingWindow::new(period),
            b_window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn update_pair(&mut self, time: DateTime, a: Decimal, b: Decimal) -> IndicatorResult {
        self.samples += 1;
        self.a_window.push(a);
        self.b_window.push(b);

        if !self.a_window.is_full() {
            return self.current.clone();
        }

        let n = self.period as f64;
        let mut sum_a = 0.0f64;
        let mut sum_b = 0.0f64;
        let mut sum_ab = 0.0f64;

        for i in 0..self.period {
            let av = self.a_window.get(i).and_then(|v| v.to_f64()).unwrap_or(0.0);
            let bv = self.b_window.get(i).and_then(|v| v.to_f64()).unwrap_or(0.0);
            sum_a += av;
            sum_b += bv;
            sum_ab += av * bv;
        }

        let cov = (sum_ab - (sum_a * sum_b) / n) / n;
        let v = Decimal::from_f64_retain(cov).unwrap_or(dec!(0));
        self.current = IndicatorResult::ready(v, time);
        self.current.clone()
    }
}

impl Indicator for Covariance {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.a_window.is_full()
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
        self.a_window.clear();
        self.b_window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }
}
