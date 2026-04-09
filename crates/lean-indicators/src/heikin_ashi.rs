use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone)]
pub struct HeikinAshiBar {
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
}

/// Heikin Ashi modified OHLC bars.
pub struct HeikinAshi {
    name: String,
    prev_ha_open: Decimal,
    prev_ha_close: Decimal,
    samples: usize,
    current: IndicatorResult,
    pub last_bar: Option<HeikinAshiBar>,
}

impl HeikinAshi {
    pub fn new() -> Self {
        HeikinAshi {
            name: "HA".to_string(),
            prev_ha_open: dec!(0),
            prev_ha_close: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
            last_bar: None,
        }
    }

    pub fn compute_bar(&mut self, bar: &TradeBar) -> HeikinAshiBar {
        self.samples += 1;
        let ha_close = (bar.open + bar.high + bar.low + bar.close) / dec!(4);

        let ha_open = if self.samples == 1 {
            (bar.open + bar.close) / dec!(2)
        } else {
            (self.prev_ha_open + self.prev_ha_close) / dec!(2)
        };

        let ha_high = bar.high.max(ha_open).max(ha_close);
        let ha_low = bar.low.min(ha_open).min(ha_close);

        self.prev_ha_open = ha_open;
        self.prev_ha_close = ha_close;

        if self.samples > 1 {
            self.current = IndicatorResult::ready(ha_close, bar.time);
        }

        let result = HeikinAshiBar { open: ha_open, high: ha_high, low: ha_low, close: ha_close };
        self.last_bar = Some(result.clone());
        result
    }
}

impl Default for HeikinAshi {
    fn default() -> Self { Self::new() }
}

impl Indicator for HeikinAshi {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples > 1 }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { 2 }

    fn reset(&mut self) {
        self.prev_ha_open = dec!(0);
        self.prev_ha_close = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
        self.last_bar = None;
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.compute_bar(bar);
        self.current.clone()
    }
}
