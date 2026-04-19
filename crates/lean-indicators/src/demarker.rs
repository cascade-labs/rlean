use crate::{
    indicator::{Indicator, IndicatorResult},
    sma::Sma,
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// DeMarker Indicator. DEM = MA(DeMax) / (MA(DeMax) + MA(DeMin)).
pub struct DeMarker {
    name: String,
    period: usize,
    max_ma: Sma,
    min_ma: Sma,
    last_high: Decimal,
    last_low: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl DeMarker {
    pub fn new(period: usize) -> Self {
        DeMarker {
            name: format!("DEM({})", period),
            period,
            max_ma: Sma::new(period),
            min_ma: Sma::new(period),
            last_high: dec!(0),
            last_low: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for DeMarker {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.max_ma.is_ready() && self.min_ma.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.period
    }

    fn reset(&mut self) {
        self.max_ma.reset();
        self.min_ma.reset();
        self.last_high = dec!(0);
        self.last_low = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let de_max = if self.samples > 1 {
            (bar.high - self.last_high).max(dec!(0))
        } else {
            dec!(0)
        };
        let de_min = if self.samples > 1 {
            (self.last_low - bar.low).max(dec!(0))
        } else {
            dec!(0)
        };

        self.max_ma.update_price(bar.time, de_max);
        self.min_ma.update_price(bar.time, de_min);
        self.last_high = bar.high;
        self.last_low = bar.low;

        if self.is_ready() {
            let max_v = self.max_ma.current().value;
            let min_v = self.min_ma.current().value;
            let denom = max_v + min_v;
            let v = if denom > dec!(0) {
                max_v / denom
            } else {
                dec!(0)
            };
            self.current = IndicatorResult::ready(v, bar.time);
        }

        self.current.clone()
    }
}
