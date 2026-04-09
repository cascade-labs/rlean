use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Chande Momentum Oscillator (CMO).
/// (sum_up - sum_down) / (sum_up + sum_down) * 100
pub struct Cmo {
    name: String,
    period: usize,
    prev_value: Option<Decimal>,
    prev_gain: Decimal,
    prev_loss: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Cmo {
    pub fn new(period: usize) -> Self {
        Cmo {
            name: format!("CMO({})", period),
            period,
            prev_value: None,
            prev_gain: dec!(0),
            prev_loss: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Cmo {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples > self.period }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period + 1 }

    fn reset(&mut self) {
        self.prev_value = None;
        self.prev_gain = dec!(0);
        self.prev_loss = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if self.samples == 1 {
            self.prev_value = Some(value);
            return self.current.clone();
        }

        let prev = self.prev_value.unwrap_or(value);
        let diff = value - prev;
        self.prev_value = Some(value);

        if self.samples > self.period + 1 {
            self.prev_loss *= Decimal::from(self.period - 1);
            self.prev_gain *= Decimal::from(self.period - 1);
        }

        if diff < dec!(0) {
            self.prev_loss -= diff;
        } else {
            self.prev_gain += diff;
        }

        if !self.is_ready() {
            return self.current.clone();
        }

        self.prev_loss /= Decimal::from(self.period);
        self.prev_gain /= Decimal::from(self.period);

        let sum = self.prev_gain + self.prev_loss;
        let v = if sum != dec!(0) {
            dec!(100) * (self.prev_gain - self.prev_loss) / sum
        } else {
            dec!(0)
        };
        self.current = IndicatorResult::ready(v, time);
        self.current.clone()
    }
}
