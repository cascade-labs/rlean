use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// McGinley Dynamic. Adaptive MA.
/// After warm-up (SMA seed): prev + (price - prev) / (n * (price/prev)^4)
pub struct McGinley {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    rolling_sum: Decimal,
    current_value: Price,
    samples: usize,
    current: IndicatorResult,
}

impl McGinley {
    pub fn new(period: usize) -> Self {
        McGinley {
            name: format!("MGD({})", period),
            period,
            window: RollingWindow::new(period),
            rolling_sum: dec!(0),
            current_value: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for McGinley {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= self.period
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
        self.rolling_sum = dec!(0);
        self.current_value = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        // maintain rolling sum for SMA seed
        if self.window.is_full() {
            if let Some(oldest) = self.window.oldest() {
                self.rolling_sum -= *oldest;
            }
        }
        self.window.push(value);
        self.rolling_sum += value;

        if !self.is_ready() {
            return self.current.clone();
        }

        if self.samples == self.period {
            // seed: SMA
            self.current_value = self.rolling_sum / Decimal::from(self.period);
        } else if self.current_value == dec!(0) || value == dec!(0) {
            // no change
        } else {
            let n = Decimal::from(self.period);
            let ratio_f = (value / self.current_value).to_f64().unwrap_or(1.0);
            let denom_f = ratio_f.powi(4);
            let denom = Decimal::from_f64_retain(denom_f).unwrap_or(dec!(1)) * n;
            if denom != dec!(0) {
                self.current_value = self.current_value + (value - self.current_value) / denom;
            }
        }

        self.current = IndicatorResult::ready(self.current_value, time);
        self.current.clone()
    }
}
