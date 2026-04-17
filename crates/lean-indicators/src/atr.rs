use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Average True Range (Wilder smoothing).
pub struct Atr {
    name: String,
    period: usize,
    prev_close: Option<Price>,
    current_atr: Price,
    samples: usize,
    initial_trs: Vec<Price>,
    current: IndicatorResult,
}

impl Atr {
    pub fn new(period: usize) -> Self {
        Atr {
            name: format!("ATR({})", period),
            period,
            prev_close: None,
            current_atr: dec!(0),
            samples: 0,
            initial_trs: Vec::with_capacity(period),
            current: IndicatorResult::not_ready(),
        }
    }

    fn true_range(&self, high: Price, low: Price, prev_close: Option<Price>) -> Price {
        let hl = high - low;
        match prev_close {
            Some(pc) => {
                let hc = (high - pc).abs();
                let lc = (low - pc).abs();
                hl.max(hc).max(lc)
            }
            None => hl,
        }
    }
}

impl Indicator for Atr {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > self.period
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
        self.prev_close = None;
        self.current_atr = dec!(0);
        self.samples = 0;
        self.initial_trs.clear();
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        // ATR requires high/low — use update_bar instead
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        let tr = self.true_range(bar.high, bar.low, self.prev_close);
        self.prev_close = Some(bar.close);

        if self.samples <= self.period {
            self.initial_trs.push(tr);
            if self.samples == self.period {
                let n = Decimal::from(self.period);
                self.current_atr = self.initial_trs.iter().sum::<Price>() / n;
            }
        } else {
            // Wilder smoothing
            let n = Decimal::from(self.period);
            self.current_atr = (self.current_atr * (n - dec!(1)) + tr) / n;
            self.current = IndicatorResult::ready(self.current_atr, bar.time);
        }

        self.current.clone()
    }
}
