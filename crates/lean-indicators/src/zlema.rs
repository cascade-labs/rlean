use crate::{indicator::{Indicator, IndicatorResult}, ema::Ema, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Zero Lag Exponential Moving Average.
/// ZLEMA(price) = EMA(2*price - price[lag])
/// lag = round((period-1)/2)
pub struct Zlema {
    name: String,
    period: usize,
    lag: usize,
    ema: Ema,
    delay: RollingWindow<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Zlema {
    pub fn new(period: usize) -> Self {
        let lag = ((period - 1) as f64 / 2.0).round() as usize;
        Zlema {
            name: format!("ZLEMA({})", period),
            period,
            lag,
            ema: Ema::new(period),
            delay: RollingWindow::new(lag + 1),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Zlema {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.ema.is_ready() && self.delay.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period + self.lag }

    fn reset(&mut self) {
        self.ema.reset();
        self.delay.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.delay.push(value);

        if self.delay.is_full() {
            let lagged = self.delay.oldest().copied().unwrap_or(dec!(0));
            let adjusted = dec!(2) * value - lagged;
            let r = self.ema.update_price(time, adjusted);
            if r.is_ready() {
                self.current = IndicatorResult::ready(r.value, time);
            }
        }

        self.current.clone()
    }
}
