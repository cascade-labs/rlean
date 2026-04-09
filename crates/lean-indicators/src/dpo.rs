use crate::{indicator::{Indicator, IndicatorResult}, sma::Sma, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Detrended Price Oscillator. close[lag] - SMA(n).
/// lag = n/2 + 1
pub struct Dpo {
    name: String,
    period: usize,
    lag: usize,
    sma: Sma,
    lag_buf: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Dpo {
    pub fn new(period: usize) -> Self {
        let lag = period / 2 + 1;
        Dpo {
            name: format!("DPO({})", period),
            period,
            lag,
            sma: Sma::new(period),
            lag_buf: RollingWindow::new(lag),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Dpo {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.sma.is_ready() && self.lag_buf.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.sma.reset();
        self.lag_buf.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.lag_buf.push(value);
        let rs = self.sma.update_price(time, value);

        if rs.is_ready() && self.lag_buf.is_full() {
            let lagged = self.lag_buf.oldest().copied().unwrap_or(dec!(0));
            let v = lagged - rs.value;
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
