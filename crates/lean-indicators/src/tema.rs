use crate::{
    ema::Ema,
    indicator::{Indicator, IndicatorResult},
};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Triple Exponential Moving Average. TEMA = 3*EMA1 - 3*EMA2 + EMA3.
pub struct Tema {
    name: String,
    period: usize,
    ema1: Ema,
    ema2: Ema,
    ema3: Ema,
    samples: usize,
    current: IndicatorResult,
}

impl Tema {
    pub fn new(period: usize) -> Self {
        Tema {
            name: format!("TEMA({})", period),
            period,
            ema1: Ema::new(period),
            ema2: Ema::new(period),
            ema3: Ema::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Tema {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.ema3.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        3 * (self.period - 1) + 1
    }

    fn reset(&mut self) {
        self.ema1.reset();
        self.ema2.reset();
        self.ema3.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let r1 = self.ema1.update_price(time, value);

        if r1.is_ready() {
            let r2 = self.ema2.update_price(time, r1.value);
            if r2.is_ready() {
                self.ema3.update_price(time, r2.value);
            }
        }

        if self.is_ready() {
            let e1 = self.ema1.current().value;
            let e2 = self.ema2.current().value;
            let e3 = self.ema3.current().value;
            let v = dec!(3) * e1 - dec!(3) * e2 + e3;
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
