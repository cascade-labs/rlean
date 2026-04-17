use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Commodity Channel Index.
pub struct Cci {
    name: String,
    period: usize,
    constant: Decimal,
    window: RollingWindow<Price>, // typical prices
    samples: usize,
    current: IndicatorResult,
}

impl Cci {
    pub fn new(period: usize) -> Self {
        Cci {
            name: format!("CCI({})", period),
            period,
            constant: dec!(0.015),
            window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Cci {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.window.is_full()
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
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        let typical = (bar.high + bar.low + bar.close) / dec!(3);
        self.window.push(typical);

        if self.window.is_full() {
            let n = Decimal::from(self.period);
            let mean: Price = self.window.iter().sum::<Price>() / n;
            let mean_dev: Price = self.window.iter().map(|&p| (p - mean).abs()).sum::<Price>() / n;

            let cci = if mean_dev.is_zero() {
                dec!(0)
            } else {
                (typical - mean) / (self.constant * mean_dev)
            };

            self.current = IndicatorResult::ready(cci, bar.time);
        }

        self.current.clone()
    }
}
