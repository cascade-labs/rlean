use crate::{ema::Ema, indicator::{Indicator, IndicatorResult}};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// MACD = Fast EMA - Slow EMA, Signal = EMA(MACD, signal_period).
pub struct Macd {
    name: String,
    fast_ema: Ema,
    slow_ema: Ema,
    signal_ema: Ema,
    samples: usize,
    pub macd_line: Price,
    pub signal_line: Price,
    pub histogram: Price,
    current: IndicatorResult,
}

impl Macd {
    pub fn new(fast: usize, slow: usize, signal: usize) -> Self {
        Macd {
            name: format!("MACD({},{},{})", fast, slow, signal),
            fast_ema: Ema::new(fast),
            slow_ema: Ema::new(slow),
            signal_ema: Ema::new(signal),
            samples: 0,
            macd_line: dec!(0),
            signal_line: dec!(0),
            histogram: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Macd {
    fn name(&self) -> &str { &self.name }

    fn is_ready(&self) -> bool {
        self.slow_ema.is_ready() && self.signal_ema.is_ready()
    }

    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize {
        self.slow_ema.warm_up_period() + self.signal_ema.warm_up_period()
    }

    fn reset(&mut self) {
        self.fast_ema.reset();
        self.slow_ema.reset();
        self.signal_ema.reset();
        self.samples = 0;
        self.macd_line = dec!(0);
        self.signal_line = dec!(0);
        self.histogram = dec!(0);
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        let fast_result = self.fast_ema.update_price(time, value);
        let slow_result = self.slow_ema.update_price(time, value);

        if slow_result.is_ready() {
            self.macd_line = fast_result.value - slow_result.value;
            let sig_result = self.signal_ema.update_price(time, self.macd_line);

            if sig_result.is_ready() {
                self.signal_line = sig_result.value;
                self.histogram = self.macd_line - self.signal_line;
                self.current = IndicatorResult::ready(self.macd_line, time);
            }
        }

        self.current.clone()
    }
}
