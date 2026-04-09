use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// MidPoint = (Highest + Lowest) / 2 over n bars.
pub struct MidPoint {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl MidPoint {
    pub fn new(period: usize) -> Self {
        MidPoint {
            name: format!("MIDPOINT({})", period),
            period,
            window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for MidPoint {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples >= self.period }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.window.push(value);

        if self.window.is_full() {
            let highest = self.window.iter().copied().fold(Decimal::MIN, |a, b| if b > a { b } else { a });
            let lowest = self.window.iter().copied().fold(Decimal::MAX, |a, b| if b < a { b } else { a });
            self.current = IndicatorResult::ready((highest + lowest) / dec!(2), time);
        }

        self.current.clone()
    }
}
