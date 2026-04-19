use crate::{
    atr::Atr,
    indicator::{Indicator, IndicatorResult},
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal_macros::dec;

/// Normalized Average True Range. ATR / close * 100.
pub struct Natr {
    name: String,
    period: usize,
    atr: Atr,
    samples: usize,
    current: IndicatorResult,
}

impl Natr {
    pub fn new(period: usize) -> Self {
        Natr {
            name: format!("NATR({})", period),
            period,
            atr: Atr::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Natr {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.atr.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.period + 1
    }

    fn reset(&mut self) {
        self.atr.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let r = self.atr.update_bar(bar);
        if r.is_ready() && bar.close != dec!(0) {
            let v = r.value / bar.close * dec!(100);
            self.current = IndicatorResult::ready(v, bar.time);
        }
        self.current.clone()
    }
}
