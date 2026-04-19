use crate::{
    indicator::{Indicator, IndicatorResult},
    sma::Sma,
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Ease of Movement Value.
/// MID = midpoint delta; RATIO = (vol/scale)/(high-low); EMV = MID/RATIO; return SMA(EMV)
pub struct EaseOfMovement {
    name: String,
    sma: Sma,
    scale: Decimal,
    prev_high: Decimal,
    prev_low: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl EaseOfMovement {
    pub fn new(period: usize, scale: usize) -> Self {
        EaseOfMovement {
            name: format!("EMV({},{})", period, scale),
            sma: Sma::new(period),
            scale: Decimal::from(scale),
            prev_high: dec!(0),
            prev_low: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Default for EaseOfMovement {
    fn default() -> Self {
        Self::new(1, 10000)
    }
}

impl Indicator for EaseOfMovement {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.sma.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.sma.warm_up_period()
    }

    fn reset(&mut self) {
        self.sma.reset();
        self.prev_high = dec!(0);
        self.prev_low = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;

        let emv = if (self.prev_high == dec!(0) && self.prev_low == dec!(0))
            || bar.volume == dec!(0)
            || bar.high == bar.low
        {
            dec!(0)
        } else {
            let mid = (bar.high + bar.low) / dec!(2) - (self.prev_high + self.prev_low) / dec!(2);
            let ratio = (bar.volume / self.scale) / (bar.high - bar.low);
            if ratio == dec!(0) {
                dec!(0)
            } else {
                mid / ratio
            }
        };

        self.prev_high = bar.high;
        self.prev_low = bar.low;

        let r = self.sma.update_price(bar.time, emv);
        if r.is_ready() {
            self.current = IndicatorResult::ready(r.value, bar.time);
        }

        self.current.clone()
    }
}
