use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Rolling maximum over n bars.
pub struct Maximum {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    pub periods_since_max: usize,
    current_max: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Maximum {
    pub fn new(period: usize) -> Self {
        Maximum {
            name: format!("MAX({})", period),
            period,
            window: RollingWindow::new(period),
            periods_since_max: 0,
            current_max: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Maximum {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples >= self.period }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.window.clear();
        self.periods_since_max = 0;
        self.current_max = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if self.samples == 1 || value >= self.current_max {
            self.current_max = value;
            self.periods_since_max = 0;
        } else if self.periods_since_max >= self.period - 1 || (self.window.is_full() && self.window.oldest().copied().unwrap_or(dec!(0)) >= self.current_max) {
            // Need to rescan
            self.window.push(value);
            let (max_val, max_idx) = self.window.iter().enumerate()
                .fold((dec!(0), 0), |(mv, mi), (i, &v)| {
                    if i == 0 || v >= mv { (v, i) } else { (mv, mi) }
                });
            self.current_max = max_val;
            self.periods_since_max = max_idx;
            if self.is_ready() {
                self.current = IndicatorResult::ready(self.current_max, time);
            }
            return self.current.clone();
        } else {
            self.periods_since_max += 1;
        }

        self.window.push(value);

        if self.is_ready() {
            self.current = IndicatorResult::ready(self.current_max, time);
        }

        self.current.clone()
    }
}
