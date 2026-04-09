use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Exponential Moving Average.
pub struct Ema {
    name: String,
    period: usize,
    multiplier: Decimal,  // 2 / (period + 1)
    samples: usize,
    current_value: Price,
    current: IndicatorResult,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        let mult = Decimal::from(2) / Decimal::from(period + 1);
        Ema {
            name: format!("EMA({})", period),
            period,
            multiplier: mult,
            samples: 0,
            current_value: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn multiplier(&self) -> Decimal { self.multiplier }
}

impl Indicator for Ema {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples >= self.period }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.samples = 0;
        self.current_value = dec!(0);
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if self.samples == 1 {
            // Seed with first value
            self.current_value = value;
        } else {
            // EMA = (value - prev_ema) * multiplier + prev_ema
            self.current_value = (value - self.current_value) * self.multiplier + self.current_value;
        }

        if self.is_ready() {
            self.current = IndicatorResult::ready(self.current_value, time);
        }

        self.current.clone()
    }
}
