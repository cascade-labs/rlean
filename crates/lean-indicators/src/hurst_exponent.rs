use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;

/// Hurst Exponent using R/S log-log regression.
pub struct HurstExponent {
    name: String,
    period: usize,
    max_lag: usize,
    window: RollingWindow<Price>,
    time_lags: Vec<usize>,
    sum_x: f64,
    sum_x2: f64,
    samples: usize,
    current: IndicatorResult,
}

impl HurstExponent {
    pub fn new(period: usize, max_lag: usize) -> Self {
        assert!(max_lag >= 3, "maxLag must be >= 3");
        let mut time_lags = Vec::new();
        let mut sum_x = 0.0f64;
        let mut sum_x2 = 0.0f64;
        for i in 2..=max_lag {
            let log_lag = (i as f64).ln();
            time_lags.push(i);
            sum_x += log_lag;
            sum_x2 += log_lag * log_lag;
        }
        HurstExponent {
            name: format!("HE({},{})", period, max_lag),
            period,
            max_lag,
            window: RollingWindow::new(period),
            time_lags,
            sum_x,
            sum_x2,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default(period: usize) -> Self {
        Self::new(period, 20)
    }
}

impl Indicator for HurstExponent {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.window.is_full() }
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

        if !self.is_ready() {
            return self.current.clone();
        }

        let n = self.time_lags.len() as f64;
        let mut sum_y = 0.0f64;
        let mut sum_xy = 0.0f64;

        for &lag in &self.time_lags {
            let count = self.window.len().saturating_sub(lag);
            let mut mean = 0.0f64;
            let mut sum_sq = 0.0f64;

            for i in 0..count {
                let a = self.window.get(i).and_then(|v| v.to_f64()).unwrap_or(0.0);
                let b = self.window.get(i + lag).and_then(|v| v.to_f64()).unwrap_or(0.0);
                let diff = b - a;
                sum_sq += diff * diff;
                mean += diff;
            }

            let std_dev = if count > 0 {
                mean /= count as f64;
                let variance = sum_sq / count as f64 - mean * mean;
                variance.max(0.0).sqrt()
            } else {
                0.0
            };

            let log_tau = if std_dev == 0.0 { 0.0 } else { std_dev.ln() };
            let log_lag = (lag as f64).ln();
            sum_y += log_tau;
            sum_xy += log_lag * log_tau;
        }

        let hurst = (n * sum_xy - self.sum_x * sum_y) / (n * self.sum_x2 - self.sum_x * self.sum_x);
        let v = Decimal::from_f64_retain(hurst).unwrap_or(dec!(0));
        self.current = IndicatorResult::ready(v, time);
        self.current.clone()
    }
}
