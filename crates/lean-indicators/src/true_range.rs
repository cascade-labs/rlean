use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;

/// True Range = max(H-L, |H-prevC|, |L-prevC|).
pub struct TrueRange {
    name: String,
    prev_close: Option<Decimal>,
    samples: usize,
    current: IndicatorResult,
}

impl TrueRange {
    pub fn new() -> Self {
        TrueRange {
            name: "TR".to_string(),
            prev_close: None,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn compute(bar: &TradeBar, prev_close: Option<Decimal>) -> Decimal {
        let hl = bar.high - bar.low;
        match prev_close {
            None => hl,
            Some(pc) => {
                let h_pc = (bar.high - pc).abs();
                let l_pc = (bar.low - pc).abs();
                hl.max(h_pc).max(l_pc)
            }
        }
    }
}

impl Default for TrueRange {
    fn default() -> Self {
        Self::new()
    }
}

impl Indicator for TrueRange {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= 1
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
        self.prev_close = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let tr = Self::compute(bar, self.prev_close);
        self.prev_close = Some(bar.close);
        self.current = IndicatorResult::ready(tr, bar.time);
        self.current.clone()
    }
}
