use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

pub struct Obv {
    name: String,
    obv: Price,
    prev_close: Option<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl Obv {
    pub fn new() -> Self {
        Obv { name: "OBV".into(), obv: dec!(0), prev_close: None, samples: 0, current: IndicatorResult::not_ready() }
    }
}

impl Default for Obv { fn default() -> Self { Obv::new() } }

impl Indicator for Obv {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples > 0 }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { 1 }
    fn reset(&mut self) { self.obv = dec!(0); self.prev_close = None; self.samples = 0; self.current = IndicatorResult::not_ready(); }
    fn update_price(&mut self, _: DateTime, _: Price) -> IndicatorResult { self.current.clone() }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        if let Some(prev) = self.prev_close {
            if bar.close > prev { self.obv += bar.volume; }
            else if bar.close < prev { self.obv -= bar.volume; }
        }
        self.prev_close = Some(bar.close);
        self.current = IndicatorResult::ready(self.obv, bar.time);
        self.current.clone()
    }
}
