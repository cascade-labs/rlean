use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// SuperTrend indicator using Wilder ATR.
pub struct SuperTrend {
    name: String,
    period: usize,
    multiplier: Decimal,
    // ATR state (Wilder smoothing)
    atr: Decimal,
    atr_samples: usize,
    initial_trs: Vec<Decimal>,
    prev_close: Option<Decimal>,
    // SuperTrend state
    super_trend: Decimal,
    prev_super: Decimal,
    trailing_upper: Decimal,
    trailing_lower: Decimal,
    prev_trailing_upper: Decimal,
    prev_trailing_lower: Decimal,
    initialized: bool,
    samples: usize,
    current: IndicatorResult,
}

impl SuperTrend {
    pub fn new(period: usize, multiplier: Decimal) -> Self {
        SuperTrend {
            name: format!("SuperTrend({},{})", period, multiplier),
            period,
            multiplier,
            atr: dec!(0),
            atr_samples: 0,
            initial_trs: Vec::with_capacity(period),
            prev_close: None,
            super_trend: dec!(0),
            prev_super: dec!(-1),
            trailing_upper: dec!(0),
            trailing_lower: dec!(0),
            prev_trailing_upper: dec!(0),
            prev_trailing_lower: dec!(0),
            initialized: false,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    fn true_range(&self, high: Decimal, low: Decimal, prev_close: Option<Decimal>) -> Decimal {
        let hl = high - low;
        match prev_close {
            Some(pc) => hl.max((high - pc).abs()).max((low - pc).abs()),
            None => hl,
        }
    }
}

impl Indicator for SuperTrend {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.initialized
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
        self.atr = dec!(0);
        self.atr_samples = 0;
        self.initial_trs.clear();
        self.prev_close = None;
        self.super_trend = dec!(0);
        self.prev_super = dec!(-1);
        self.trailing_upper = dec!(0);
        self.trailing_lower = dec!(0);
        self.prev_trailing_upper = dec!(0);
        self.prev_trailing_lower = dec!(0);
        self.initialized = false;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let tr = self.true_range(bar.high, bar.low, self.prev_close);

        if self.atr_samples < self.period {
            self.atr_samples += 1;
            self.initial_trs.push(tr);
            if self.atr_samples == self.period {
                let n = Decimal::from(self.period);
                self.atr = self.initial_trs.iter().sum::<Decimal>() / n;
            }
            self.prev_close = Some(bar.close);
            return self.current.clone();
        }

        // Wilder smoothing for ATR
        let n = Decimal::from(self.period);
        self.atr = (self.atr * (n - dec!(1)) + tr) / n;
        self.initialized = true;

        let basic_lower = (bar.high + bar.low) / dec!(2) - self.multiplier * self.atr;
        let basic_upper = (bar.high + bar.low) / dec!(2) + self.multiplier * self.atr;

        let prev_close = self.prev_close.unwrap_or(bar.close);
        self.trailing_lower =
            if basic_lower > self.prev_trailing_lower || prev_close < self.prev_trailing_lower {
                basic_lower
            } else {
                self.prev_trailing_lower
            };
        self.trailing_upper =
            if basic_upper < self.prev_trailing_upper || prev_close > self.prev_trailing_upper {
                basic_upper
            } else {
                self.prev_trailing_upper
            };

        if self.prev_super == dec!(-1) || self.prev_super == self.prev_trailing_upper {
            self.super_trend = if bar.close <= self.trailing_upper {
                self.trailing_upper
            } else {
                self.trailing_lower
            };
        } else if self.prev_super == self.prev_trailing_lower {
            self.super_trend = if bar.close >= self.trailing_lower {
                self.trailing_lower
            } else {
                self.trailing_upper
            };
        }

        self.prev_close = Some(bar.close);
        self.prev_super = self.super_trend;
        self.prev_trailing_lower = self.trailing_lower;
        self.prev_trailing_upper = self.trailing_upper;

        self.current = IndicatorResult::ready(self.super_trend, bar.time);
        self.current.clone()
    }
}
