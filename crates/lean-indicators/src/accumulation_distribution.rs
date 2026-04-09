use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Accumulation/Distribution. Cumulative MFM*volume.
pub struct AccumulationDistribution {
    name: String,
    ad: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl AccumulationDistribution {
    pub fn new() -> Self {
        AccumulationDistribution {
            name: "AD".to_string(),
            ad: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Default for AccumulationDistribution {
    fn default() -> Self { Self::new() }
}

impl Indicator for AccumulationDistribution {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples > 0 }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { 1 }

    fn reset(&mut self) {
        self.ad = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let range = bar.high - bar.low;
        let mfm = if range > dec!(0) {
            ((bar.close - bar.low) - (bar.high - bar.close)) / range
        } else {
            dec!(0)
        };
        self.ad += mfm * bar.volume;
        self.current = IndicatorResult::ready(self.ad, bar.time);
        self.current.clone()
    }
}
