use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Fisher Transform. Normalizes price into a Gaussian distribution.
/// FT = 0.5 * ln((1 + x) / (1 - x)) where x is normalized midpoint.
pub struct FisherTransform {
    name: String,
    period: usize,
    high_window: RollingWindow<Decimal>,
    low_window: RollingWindow<Decimal>,
    samples: usize,
    current: IndicatorResult,
}

impl FisherTransform {
    pub fn new(period: usize) -> Self {
        FisherTransform {
            name: format!("FISH({})", period),
            period,
            high_window: RollingWindow::new(period),
            low_window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    fn highest(&self) -> Decimal {
        self.high_window
            .iter()
            .copied()
            .fold(Decimal::MIN, |a, b| if b > a { b } else { a })
    }

    fn lowest(&self) -> Decimal {
        self.low_window
            .iter()
            .copied()
            .fold(Decimal::MAX, |a, b| if b < a { b } else { a })
    }
}

impl Default for FisherTransform {
    fn default() -> Self {
        Self::new(10)
    }
}

impl Indicator for FisherTransform {
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
        self.high_window.clear();
        self.low_window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let mid = (bar.high + bar.low) / dec!(2);
        self.high_window.push(bar.high);
        self.low_window.push(bar.low);

        if self.high_window.is_full() {
            let highest = self.highest();
            let lowest = self.lowest();
            let range = highest - lowest;

            let x = if range > dec!(0) {
                let raw = (dec!(2) * (mid - lowest) / range) - dec!(1);
                // Clamp to avoid infinity
                if raw >= dec!(1) {
                    dec!(0.999)
                } else if raw <= dec!(-1) {
                    dec!(-0.999)
                } else {
                    raw
                }
            } else {
                dec!(0)
            };

            let x_f = x.to_f64().unwrap_or(0.0);
            let ft = 0.5 * ((1.0 + x_f) / (1.0 - x_f)).ln();
            let ft_dec = Decimal::from_f64_retain(ft).unwrap_or(dec!(0));
            self.current = IndicatorResult::ready(ft_dec, bar.time);
        }

        self.current.clone()
    }
}
