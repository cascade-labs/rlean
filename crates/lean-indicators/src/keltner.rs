use crate::{atr::Atr, ema::Ema, indicator::{Indicator, IndicatorResult}};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub struct KeltnerChannel {
    name: String,
    ema: Ema,
    atr: Atr,
    multiplier: Decimal,
    samples: usize,
    pub upper: Price,
    pub lower: Price,
    pub middle: Price,
    current: IndicatorResult,
}

impl KeltnerChannel {
    pub fn new(period: usize, multiplier: Decimal) -> Self {
        KeltnerChannel {
            name: format!("KC({},{})", period, multiplier),
            ema: Ema::new(period),
            atr: Atr::new(period),
            multiplier,
            samples: 0,
            upper: dec!(0),
            lower: dec!(0),
            middle: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn standard(period: usize) -> Self { Self::new(period, dec!(2)) }
}

impl Indicator for KeltnerChannel {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.ema.is_ready() && self.atr.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.ema.warm_up_period() + 1 }
    fn reset(&mut self) {
        self.ema.reset(); self.atr.reset();
        self.samples = 0; self.current = IndicatorResult::not_ready();
    }
    fn update_price(&mut self, _: DateTime, _: Price) -> IndicatorResult { self.current.clone() }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        let ema_r = self.ema.update_price(bar.time, bar.close);
        let atr_r = self.atr.update_bar(bar);

        if ema_r.is_ready() && atr_r.is_ready() {
            self.middle = ema_r.value;
            let band = self.multiplier * atr_r.value;
            self.upper = self.middle + band;
            self.lower = self.middle - band;
            self.current = IndicatorResult::ready(self.middle, bar.time);
        }

        self.current.clone()
    }
}
