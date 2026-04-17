use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

pub struct MoneyFlowIndex {
    name: String,
    period: usize,
    window: RollingWindow<(Price, Price)>, // (positive_mf, negative_mf)
    prev_typical: Option<Price>,
    samples: usize,
    current: IndicatorResult,
}

impl MoneyFlowIndex {
    pub fn new(period: usize) -> Self {
        MoneyFlowIndex {
            name: format!("MFI({})", period),
            period,
            window: RollingWindow::new(period),
            prev_typical: None,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for MoneyFlowIndex {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.window.is_full()
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
        self.window.clear();
        self.prev_typical = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }
    fn update_price(&mut self, _: DateTime, _: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        let typical = (bar.high + bar.low + bar.close) / dec!(3);
        let raw_mf = typical * bar.volume;

        let (pos_mf, neg_mf) = match self.prev_typical {
            Some(pt) if typical > pt => (raw_mf, dec!(0)),
            Some(pt) if typical < pt => (dec!(0), raw_mf),
            _ => (dec!(0), dec!(0)),
        };

        self.prev_typical = Some(typical);
        self.window.push((pos_mf, neg_mf));

        if self.window.is_full() {
            let total_pos: Price = self.window.iter().map(|(p, _)| *p).sum();
            let total_neg: Price = self.window.iter().map(|(_, n)| *n).sum();
            let mfi = if total_neg.is_zero() {
                dec!(100)
            } else {
                let mfr = total_pos / total_neg;
                dec!(100) - dec!(100) / (dec!(1) + mfr)
            };
            self.current = IndicatorResult::ready(mfi, bar.time);
        }

        self.current.clone()
    }
}
