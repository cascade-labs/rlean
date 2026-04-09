use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Rolling population variance.
pub struct Variance {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    rolling_sum: Decimal,
    rolling_sum_sq: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Variance {
    pub fn new(period: usize) -> Self {
        Variance {
            name: format!("VAR({})", period),
            period,
            window: RollingWindow::new(period),
            rolling_sum: dec!(0),
            rolling_sum_sq: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Variance {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.window.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.window.clear();
        self.rolling_sum = dec!(0);
        self.rolling_sum_sq = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if self.window.is_full() {
            if let Some(oldest) = self.window.oldest() {
                self.rolling_sum -= *oldest;
                self.rolling_sum_sq -= *oldest * *oldest;
            }
        }

        self.window.push(value);
        self.rolling_sum += value;
        self.rolling_sum_sq += value * value;

        if self.samples < 2 {
            return self.current.clone();
        }

        let n = Decimal::from(self.window.len());
        let mean = self.rolling_sum / n;
        let mean_sq = self.rolling_sum_sq / n;
        let var = (mean_sq - mean * mean).max(dec!(0));

        if self.is_ready() {
            self.current = IndicatorResult::ready(var, time);
        }

        self.current.clone()
    }
}
