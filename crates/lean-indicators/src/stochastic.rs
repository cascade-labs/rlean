use crate::{
    indicator::{Indicator, IndicatorResult},
    sma::Sma,
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Stochastic Oscillator (%K and %D).
pub struct Stochastic {
    name: String,
    k_period: usize,
    d_period: usize,
    window: RollingWindow<(Price, Price, Price)>, // (high, low, close)
    d_sma: Sma,
    samples: usize,
    pub k: Price,
    pub d: Price,
    current: IndicatorResult,
}

impl Stochastic {
    pub fn new(k_period: usize, d_period: usize) -> Self {
        Stochastic {
            name: format!("Stoch({},{})", k_period, d_period),
            k_period,
            d_period,
            window: RollingWindow::new(k_period),
            d_sma: Sma::new(d_period),
            samples: 0,
            k: dec!(0),
            d: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Stochastic {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.window.is_full() && self.d_sma.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.k_period + self.d_period - 1
    }

    fn reset(&mut self) {
        self.window.clear();
        self.d_sma.reset();
        self.samples = 0;
        self.k = dec!(0);
        self.d = dec!(0);
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        self.window.push((bar.high, bar.low, bar.close));

        if self.window.is_full() {
            let highest_high = self
                .window
                .iter()
                .map(|(h, _, _)| *h)
                .fold(dec!(0), |acc, x| acc.max(x));
            let lowest_low = self
                .window
                .iter()
                .map(|(_, l, _)| *l)
                .fold(Price::MAX, |acc, x| acc.min(x));
            let range = highest_high - lowest_low;

            self.k = if range.is_zero() {
                dec!(50)
            } else {
                dec!(100) * (bar.close - lowest_low) / range
            };

            let d_result = self.d_sma.update_price(bar.time, self.k);
            if d_result.is_ready() {
                self.d = d_result.value;
                self.current = IndicatorResult::ready(self.k, bar.time);
            }
        }

        self.current.clone()
    }
}
