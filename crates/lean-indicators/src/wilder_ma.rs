use crate::{indicator::{Indicator, IndicatorResult}, sma::Sma};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Wilder's Moving Average. Uses 1/period smoothing.
pub struct WilderMa {
    name: String,
    period: usize,
    k: Decimal,
    sma: Sma,
    current_value: Price,
    samples: usize,
    current: IndicatorResult,
}

impl WilderMa {
    pub fn new(period: usize) -> Self {
        WilderMa {
            name: format!("WWMA({})", period),
            period,
            k: dec!(1) / Decimal::from(period),
            sma: Sma::new(period),
            current_value: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for WilderMa {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples >= self.period }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.sma.reset();
        self.current_value = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if !self.is_ready() {
            self.sma.update_price(time, value);
            self.current_value = self.sma.current().value;
        } else {
            self.current_value = value * self.k + self.current_value * (dec!(1) - self.k);
            self.current = IndicatorResult::ready(self.current_value, time);
        }

        self.current.clone()
    }
}
