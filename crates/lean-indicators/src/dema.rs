use crate::{
    ema::Ema,
    indicator::{Indicator, IndicatorResult},
};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Double Exponential Moving Average. DEMA = 2*EMA - EMA(EMA).
pub struct Dema {
    name: String,
    period: usize,
    ema1: Ema,
    ema2: Ema,
    samples: usize,
    current: IndicatorResult,
}

impl Dema {
    pub fn new(period: usize) -> Self {
        Dema {
            name: format!("DEMA({})", period),
            period,
            ema1: Ema::new(period),
            ema2: Ema::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Dema {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > 2 * (self.period - 1)
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        1 + 2 * (self.period - 1)
    }

    fn reset(&mut self) {
        self.ema1.reset();
        self.ema2.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let r1 = self.ema1.update_price(time, value);

        if r1.is_ready() {
            self.ema2.update_price(time, r1.value);
        }

        if self.is_ready() {
            let v = dec!(2) * r1.value - self.ema2.current().value;
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
