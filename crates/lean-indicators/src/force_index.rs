use crate::{indicator::{Indicator, IndicatorResult}, ema::Ema};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Force Index. (close - prev_close) * volume, smoothed by EMA.
pub struct ForceIndex {
    name: String,
    ema: Ema,
    prev_close: Option<Decimal>,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl ForceIndex {
    pub fn new(period: usize) -> Self {
        ForceIndex {
            name: format!("FI({})", period),
            ema: Ema::new(period),
            prev_close: None,
            warm_up: period + 1,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for ForceIndex {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.ema.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.ema.reset();
        self.prev_close = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        if self.samples < 2 {
            self.prev_close = Some(bar.close);
            return self.current.clone();
        }

        let prev = self.prev_close.unwrap_or(bar.close);
        let fi = (bar.close - prev) * bar.volume;
        let r = self.ema.update_price(bar.time, fi);
        if r.is_ready() {
            self.current = IndicatorResult::ready(r.value, bar.time);
        }
        self.prev_close = Some(bar.close);
        self.current.clone()
    }
}
