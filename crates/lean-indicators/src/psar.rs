use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Parabolic Stop And Reverse (PSAR).
pub struct Psar {
    name: String,
    af_start: Decimal,
    af_increment: Decimal,
    af_max: Decimal,
    // state
    is_long: bool,
    af: Decimal,
    extreme_point: Decimal,
    sar: Decimal,
    prev_high: Decimal,
    prev_low: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl Psar {
    pub fn new(af_start: Decimal, af_increment: Decimal, af_max: Decimal) -> Self {
        Psar {
            name: format!("PSAR({},{},{})", af_start, af_increment, af_max),
            af_start,
            af_increment,
            af_max,
            is_long: false,
            af: af_start,
            extreme_point: dec!(0),
            sar: dec!(0),
            prev_high: dec!(0),
            prev_low: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Default for Psar {
    fn default() -> Self {
        Self::new(dec!(0.02), dec!(0.02), dec!(0.2))
    }
}

impl Indicator for Psar {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > 1
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.is_long = false;
        self.af = self.af_start;
        self.extreme_point = dec!(0);
        self.sar = dec!(0);
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

        if self.samples == 1 {
            self.prev_high = bar.high;
            self.prev_low = bar.low;
            return self.current.clone();
        }

        if self.samples == 2 {
            // Initialize: assume bullish
            self.is_long = bar.close > self.prev_low;
            if self.is_long {
                self.sar = self.prev_low;
                self.extreme_point = bar.high;
            } else {
                self.sar = self.prev_high;
                self.extreme_point = bar.low;
            }
            self.af = self.af_start;
        } else {
            let new_sar = self.sar + self.af * (self.extreme_point - self.sar);

            if self.is_long {
                let new_sar = new_sar.min(self.prev_low).min(bar.low);
                if bar.low < new_sar {
                    // Flip to short
                    self.is_long = false;
                    self.sar = self.extreme_point;
                    self.extreme_point = bar.low;
                    self.af = self.af_start;
                } else {
                    self.sar = new_sar;
                    if bar.high > self.extreme_point {
                        self.extreme_point = bar.high;
                        self.af = (self.af + self.af_increment).min(self.af_max);
                    }
                }
            } else {
                let new_sar = new_sar.max(self.prev_high).max(bar.high);
                if bar.high > new_sar {
                    // Flip to long
                    self.is_long = true;
                    self.sar = self.extreme_point;
                    self.extreme_point = bar.high;
                    self.af = self.af_start;
                } else {
                    self.sar = new_sar;
                    if bar.low < self.extreme_point {
                        self.extreme_point = bar.low;
                        self.af = (self.af + self.af_increment).min(self.af_max);
                    }
                }
            }
        }

        self.prev_high = bar.high;
        self.prev_low = bar.low;

        if self.is_ready() {
            self.current = IndicatorResult::ready(self.sar.abs(), bar.time);
        }
        self.current.clone()
    }
}
