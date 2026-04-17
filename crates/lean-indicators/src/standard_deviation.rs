use crate::{
    indicator::{Indicator, IndicatorResult},
    variance::Variance,
};
use lean_core::{DateTime, Price};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Rolling population standard deviation (sqrt of variance).
pub struct StandardDeviation {
    name: String,
    variance: Variance,
    samples: usize,
    current: IndicatorResult,
}

impl StandardDeviation {
    pub fn new(period: usize) -> Self {
        StandardDeviation {
            name: format!("STD({})", period),
            variance: Variance::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for StandardDeviation {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.variance.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.variance.warm_up_period()
    }

    fn reset(&mut self) {
        self.variance.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let rv = self.variance.update_price(time, value);
        if rv.is_ready() {
            let var_f = rv.value.to_f64().unwrap_or(0.0);
            let std_f = var_f.sqrt();
            let std_d = Decimal::from_f64_retain(std_f).unwrap_or(rv.value);
            self.current = IndicatorResult::ready(std_d, time);
        }
        self.current.clone()
    }
}
