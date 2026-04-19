use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Kaufman Adaptive Moving Average.
pub struct Kama {
    name: String,
    period: usize,
    slow_sc: Decimal, // slow EMA smoothing constant (2/(30+1))
    diff_sc: Decimal,
    window: RollingWindow<Price>,
    prev_kama: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Kama {
    pub fn new(period: usize, fast_period: usize, slow_period: usize) -> Self {
        let slow_sc = dec!(2) / (Decimal::from(slow_period) + dec!(1));
        let fast_sc = dec!(2) / (Decimal::from(fast_period) + dec!(1));
        let diff_sc = fast_sc - slow_sc;
        Kama {
            name: format!("KAMA({},{},{})", period, fast_period, slow_period),
            period,
            slow_sc,
            diff_sc,
            window: RollingWindow::new(period),
            prev_kama: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default(period: usize) -> Self {
        Self::new(period, 2, 30)
    }
}

impl Indicator for Kama {
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
        self.prev_kama = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.window.push(value);

        if self.samples < self.period {
            return self.current.clone();
        }

        if self.samples == self.period {
            // first KAMA: use yesterday's price (oldest in window) as seed
            self.prev_kama = self.window.oldest().copied().unwrap_or(value);
        }

        // Efficiency Ratio
        let oldest = self.window.oldest().copied().unwrap_or(value);
        let direction = (value - oldest).abs();
        let mut volatility = dec!(0);
        let n = self.window.len();
        for i in 0..n.saturating_sub(1) {
            let a = self.window.get(i).copied().unwrap_or(dec!(0));
            let b = self.window.get(i + 1).copied().unwrap_or(dec!(0));
            volatility += (a - b).abs();
        }

        let er = if volatility == dec!(0) {
            dec!(0)
        } else {
            direction / volatility
        };
        let sc_base = er * self.diff_sc + self.slow_sc;
        let sc_f = sc_base.to_f64().unwrap_or(0.0).powi(2);
        let sc = Decimal::from_f64_retain(sc_f).unwrap_or(dec!(0));
        self.prev_kama = (value - self.prev_kama) * sc + self.prev_kama;

        if self.is_ready() {
            self.current = IndicatorResult::ready(self.prev_kama, time);
        }

        self.current.clone()
    }
}
