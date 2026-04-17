use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;

/// Rolling minimum over n bars.
pub struct Minimum {
    name: String,
    period: usize,
    window: RollingWindow<Price>,
    pub periods_since_min: usize,
    current_min: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Minimum {
    pub fn new(period: usize) -> Self {
        Minimum {
            name: format!("MIN({})", period),
            period,
            window: RollingWindow::new(period),
            periods_since_min: 0,
            current_min: Decimal::MAX,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Minimum {
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
        self.periods_since_min = 0;
        self.current_min = Decimal::MAX;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if self.samples == 1 || value <= self.current_min {
            self.current_min = value;
            self.periods_since_min = 0;
        } else if self.periods_since_min >= self.period - 1
            || (self.window.is_full()
                && self.window.oldest().copied().unwrap_or(Decimal::MAX) <= self.current_min)
        {
            // Need to rescan
            self.window.push(value);
            let (min_val, min_idx) =
                self.window
                    .iter()
                    .enumerate()
                    .fold((Decimal::MAX, 0), |(mv, mi), (i, &v)| {
                        if i == 0 || v <= mv {
                            (v, i)
                        } else {
                            (mv, mi)
                        }
                    });
            self.current_min = min_val;
            self.periods_since_min = min_idx;
            if self.is_ready() {
                self.current = IndicatorResult::ready(self.current_min, time);
            }
            return self.current.clone();
        } else {
            self.periods_since_min += 1;
        }

        self.window.push(value);

        if self.is_ready() {
            self.current = IndicatorResult::ready(self.current_min, time);
        }

        self.current.clone()
    }
}
