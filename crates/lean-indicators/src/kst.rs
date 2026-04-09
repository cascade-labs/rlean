use crate::{indicator::{Indicator, IndicatorResult}, sma::Sma, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Know Sure Thing (KST).
/// KST = 1*SMA(ROC1,10) + 2*SMA(ROC2,10) + 3*SMA(ROC3,10) + 4*SMA(ROC4,15)
/// Default ROC periods: 10,13,14,15; SMA periods: 10,13,15,20; signal: 9
pub struct Kst {
    name: String,
    roc1_buf: RollingWindow<Price>,
    roc2_buf: RollingWindow<Price>,
    roc3_buf: RollingWindow<Price>,
    roc4_buf: RollingWindow<Price>,
    sma1: Sma,
    sma2: Sma,
    sma3: Sma,
    sma4: Sma,
    signal_sma: Sma,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl Kst {
    pub fn new(
        roc1: usize, sma1: usize,
        roc2: usize, sma2: usize,
        roc3: usize, sma3: usize,
        roc4: usize, sma4: usize,
        signal: usize,
    ) -> Self {
        let warm_up = (roc1 + sma1).max(roc2 + sma2).max(roc3 + sma3).max(roc4 + sma4);
        Kst {
            name: format!("KST"),
            roc1_buf: RollingWindow::new(roc1 + 1),
            roc2_buf: RollingWindow::new(roc2 + 1),
            roc3_buf: RollingWindow::new(roc3 + 1),
            roc4_buf: RollingWindow::new(roc4 + 1),
            sma1: Sma::new(sma1),
            sma2: Sma::new(sma2),
            sma3: Sma::new(sma3),
            sma4: Sma::new(sma4),
            signal_sma: Sma::new(signal),
            warm_up,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default() -> Self {
        Self::new(10, 10, 13, 13, 14, 15, 15, 20, 9)
    }

    fn roc(buf: &RollingWindow<Price>) -> Decimal {
        if !buf.is_full() { return dec!(0); }
        let newest = buf.newest().copied().unwrap_or(dec!(0));
        let oldest = buf.oldest().copied().unwrap_or(dec!(0));
        if oldest == dec!(0) { return dec!(0); }
        (newest - oldest) / oldest * dec!(100)
    }
}

impl Indicator for Kst {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool {
        self.sma1.is_ready() && self.sma2.is_ready() && self.sma3.is_ready() && self.sma4.is_ready() && self.signal_sma.is_ready()
    }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.roc1_buf.clear();
        self.roc2_buf.clear();
        self.roc3_buf.clear();
        self.roc4_buf.clear();
        self.sma1.reset();
        self.sma2.reset();
        self.sma3.reset();
        self.sma4.reset();
        self.signal_sma.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        self.roc1_buf.push(value);
        self.roc2_buf.push(value);
        self.roc3_buf.push(value);
        self.roc4_buf.push(value);

        if self.roc1_buf.is_full() {
            let r = Self::roc(&self.roc1_buf);
            self.sma1.update_price(time, r);
        }
        if self.roc2_buf.is_full() {
            let r = Self::roc(&self.roc2_buf);
            self.sma2.update_price(time, r);
        }
        if self.roc3_buf.is_full() {
            let r = Self::roc(&self.roc3_buf);
            self.sma3.update_price(time, r);
        }
        if self.roc4_buf.is_full() {
            let r = Self::roc(&self.roc4_buf);
            self.sma4.update_price(time, r);
        }

        let kst = self.sma1.current().value
            + dec!(2) * self.sma2.current().value
            + dec!(3) * self.sma3.current().value
            + dec!(4) * self.sma4.current().value;

        self.signal_sma.update_price(time, kst);

        if self.is_ready() {
            self.current = IndicatorResult::ready(kst, time);
        }

        self.current.clone()
    }
}
