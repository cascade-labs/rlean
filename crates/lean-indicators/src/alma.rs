use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Arnaud Legoux Moving Average. Gaussian-weighted MA.
pub struct Alma {
    name: String,
    period: usize,
    weights: Vec<Decimal>,
    window: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Alma {
    /// sigma default 6, offset default 0.85
    pub fn new(period: usize, sigma: usize, offset: f64) -> Self {
        let m = (offset * (period as f64 - 1.0)).floor();
        let s = period as f64 / sigma as f64;

        let raw: Vec<f64> = (0..period)
            .map(|i| (-(i as f64 - m) * (i as f64 - m) / (2.0 * s * s)).exp())
            .collect();
        let sum: f64 = raw.iter().sum();

        // weights[0] = oldest, weights[period-1] = newest (reversed from raw)
        let weights: Vec<Decimal> = raw
            .iter()
            .rev()
            .map(|w| Decimal::from_f64_retain(w / sum).unwrap_or(dec!(0)))
            .collect();

        Alma {
            name: format!("ALMA({},{},{})", period, sigma, offset),
            period,
            weights,
            window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn with_defaults(period: usize) -> Self {
        Self::new(period, 6, 0.85)
    }
}

impl Indicator for Alma {
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

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.window.push(value);
        self.samples += 1;

        if self.is_ready() {
            // window[0] = newest (weight[period-1]), window[period-1] = oldest (weight[0])
            let v: Decimal = (0..self.period)
                .map(|i| {
                    let w = self.weights[self.period - 1 - i];
                    let val = self.window.get(i).copied().unwrap_or(dec!(0));
                    w * val
                })
                .sum();
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
