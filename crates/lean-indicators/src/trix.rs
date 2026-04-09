use crate::{indicator::{Indicator, IndicatorResult}, ema::Ema};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// TRIX. 1-period ROC of triple-smoothed EMA.
pub struct Trix {
    name: String,
    period: usize,
    ema1: Ema,
    ema2: Ema,
    ema3: Ema,
    prev_ema3: Option<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Trix {
    pub fn new(period: usize) -> Self {
        Trix {
            name: format!("TRIX({})", period),
            period,
            ema1: Ema::new(period),
            ema2: Ema::new(period),
            ema3: Ema::new(period),
            prev_ema3: None,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Trix {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.prev_ema3.is_some() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { 3 * (self.period - 1) + 2 }

    fn reset(&mut self) {
        self.ema1.reset();
        self.ema2.reset();
        self.ema3.reset();
        self.prev_ema3 = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let r1 = self.ema1.update_price(time, value);

        if r1.is_ready() {
            let r2 = self.ema2.update_price(time, r1.value);
            if r2.is_ready() {
                let current_ema3_before = self.ema3.current().value;
                let r3 = self.ema3.update_price(time, r2.value);
                if r3.is_ready() {
                    if let Some(p3) = self.prev_ema3 {
                        let roc = if p3 != dec!(0) {
                            (r3.value - p3) / p3 * dec!(100)
                        } else {
                            dec!(0)
                        };
                        self.current = IndicatorResult::ready(roc, time);
                    }
                    // prev_ema3 is the ema3 value from the previous step
                    self.prev_ema3 = Some(current_ema3_before);
                }
            }
        }

        self.current.clone()
    }
}
