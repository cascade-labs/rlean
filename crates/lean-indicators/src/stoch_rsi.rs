use crate::{indicator::{Indicator, IndicatorResult}, rsi::Rsi, window::RollingWindow, sma::Sma};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Stochastic RSI.
pub struct StochasticRsi {
    name: String,
    rsi: Rsi,
    rsi_window: RollingWindow<Decimal>,
    k_sma: Sma,
    d_sma: Sma,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl StochasticRsi {
    pub fn new(rsi_period: usize, stoch_period: usize, k_period: usize, d_period: usize) -> Self {
        let warm_up = rsi_period + stoch_period + k_period.max(d_period);
        StochasticRsi {
            name: format!("SRSI({},{},{},{})", rsi_period, stoch_period, k_period, d_period),
            rsi: Rsi::new(rsi_period),
            rsi_window: RollingWindow::new(stoch_period),
            k_sma: Sma::new(k_period),
            d_sma: Sma::new(d_period),
            warm_up,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default() -> Self {
        Self::new(14, 14, 3, 3)
    }
}

impl Indicator for StochasticRsi {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples >= self.warm_up }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.rsi.reset();
        self.rsi_window.clear();
        self.k_sma.reset();
        self.d_sma.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let rr = self.rsi.update_price(time, value);
        self.rsi_window.push(rr.value);

        if !self.rsi_window.is_full() {
            return self.current.clone();
        }

        let max_rsi = self.rsi_window.iter().copied().fold(Decimal::MIN, Decimal::max);
        let min_rsi = self.rsi_window.iter().copied().fold(Decimal::MAX, Decimal::min);

        let k = if max_rsi != min_rsi {
            dec!(100) * (rr.value - min_rsi) / (max_rsi - min_rsi)
        } else {
            dec!(100)
        };

        let rk = self.k_sma.update_price(time, k);
        let rd = self.d_sma.update_price(time, rk.value);

        if rk.is_ready() {
            self.current = IndicatorResult::ready(rk.value, time);
        }

        self.current.clone()
    }
}
