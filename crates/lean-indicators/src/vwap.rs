use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Volume-Weighted Average Price — resets each session.
pub struct Vwap {
    name: String,
    cumulative_pv: Price,
    cumulative_volume: Price,
    samples: usize,
    current: IndicatorResult,
}

impl Vwap {
    pub fn new() -> Self {
        Vwap {
            name: "VWAP".into(),
            cumulative_pv: dec!(0),
            cumulative_volume: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn reset_session(&mut self) {
        self.cumulative_pv = dec!(0);
        self.cumulative_volume = dec!(0);
        self.current = IndicatorResult::not_ready();
    }
}

impl Default for Vwap {
    fn default() -> Self {
        Vwap::new()
    }
}

impl Indicator for Vwap {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > 0
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        1
    }
    fn reset(&mut self) {
        self.reset_session();
        self.samples = 0;
    }
    fn update_price(&mut self, _: DateTime, _: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        let typical = (bar.high + bar.low + bar.close) / dec!(3);
        self.cumulative_pv += typical * bar.volume;
        self.cumulative_volume += bar.volume;

        if !self.cumulative_volume.is_zero() {
            let vwap = self.cumulative_pv / self.cumulative_volume;
            self.current = IndicatorResult::ready(vwap, bar.time);
        }

        self.current.clone()
    }
}
