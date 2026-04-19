use crate::{
    dema::Dema,
    indicator::{Indicator, IndicatorResult},
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Tillson T3 Moving Average. T3 = GD(GD(GD)), where GD is generalized DEMA.
/// GD(x, vf) = (vf+1)*EMA - vf*EMA(EMA)
/// T3 uses volumeFactor = 0.7 by default.
pub struct T3 {
    name: String,
    period: usize,
    gd1: Dema,
    gd2: Dema,
    gd3: Dema,
    samples: usize,
    current: IndicatorResult,
}

impl T3 {
    pub fn new(period: usize, volume_factor: Decimal) -> Self {
        T3 {
            name: format!("T3({},{})", period, volume_factor),
            period,
            gd1: Dema::new(period),
            gd2: Dema::new(period),
            gd3: Dema::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default(period: usize) -> Self {
        Self::new(period, dec!(0.7))
    }
}

impl Indicator for T3 {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > 6 * (self.period - 1)
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        1 + 6 * (self.period - 1)
    }

    fn reset(&mut self) {
        self.gd1.reset();
        self.gd2.reset();
        self.gd3.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let r1 = self.gd1.update_price(time, value);

        if r1.is_ready() {
            let r2 = self.gd2.update_price(time, r1.value);
            if r2.is_ready() {
                let r3 = self.gd3.update_price(time, r2.value);
                if r3.is_ready() && self.is_ready() {
                    self.current = IndicatorResult::ready(r3.value, time);
                }
            }
        }

        self.current.clone()
    }
}
