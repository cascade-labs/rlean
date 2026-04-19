use crate::{
    indicator::{Indicator, IndicatorResult},
    sma::Sma,
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Relative Vigor Index.
/// NUM = (a + 2*(b+c) + d)/6, DENOM = (e + 2*(f+g) + h)/6
/// RVI = SMA(NUM)/SMA(DENOM)
pub struct RelativeVigorIndex {
    name: String,
    close_sma: Sma,
    range_sma: Sma,
    prev_bars: RollingWindow<(Decimal, Decimal, Decimal, Decimal)>, // (open,high,low,close)
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl RelativeVigorIndex {
    pub fn new(period: usize) -> Self {
        RelativeVigorIndex {
            name: format!("RVI({})", period),
            close_sma: Sma::new(period),
            range_sma: Sma::new(period),
            prev_bars: RollingWindow::new(3),
            warm_up: period + 3,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for RelativeVigorIndex {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.close_sma.is_ready() && self.range_sma.is_ready()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.warm_up
    }

    fn reset(&mut self) {
        self.close_sma.reset();
        self.range_sma.reset();
        self.prev_bars.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;

        if self.prev_bars.is_full() {
            // prev_bars[0] = most recent-1, prev_bars[1] = most recent-2, prev_bars[2] = most recent-3
            let (b_o, b_h, b_l, b_c) = self.prev_bars.get(0).copied().unwrap_or_default();
            let (c_o, c_h, c_l, c_c) = self.prev_bars.get(1).copied().unwrap_or_default();
            let (d_o, d_h, d_l, d_c) = self.prev_bars.get(2).copied().unwrap_or_default();

            let a = bar.close - bar.open;
            let b = b_c - b_o;
            let c = c_c - c_o;
            let d = d_c - d_o;
            let e = bar.high - bar.low;
            let f = b_h - b_l;
            let g = c_h - c_l;
            let h = d_h - d_l;

            let num = (a + dec!(2) * (b + c) + d) / dec!(6);
            let denom = (e + dec!(2) * (f + g) + h) / dec!(6);

            let rc = self.close_sma.update_price(bar.time, num);
            let rr = self.range_sma.update_price(bar.time, denom);

            if rc.is_ready() && rr.is_ready() && rr.value != dec!(0) {
                let v = rc.value / rr.value;
                self.current = IndicatorResult::ready(v, bar.time);
            }
        }

        self.prev_bars
            .push((bar.open, bar.high, bar.low, bar.close));
        self.current.clone()
    }
}
