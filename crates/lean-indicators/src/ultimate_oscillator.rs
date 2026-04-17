use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Ultimate Oscillator (7, 14, 28 default).
pub struct UltimateOscillator {
    name: String,
    max_period: usize,
    prev_close: Option<Decimal>,
    bp1: RollingWindow<Decimal>, // buying pressure
    bp2: RollingWindow<Decimal>,
    bp3: RollingWindow<Decimal>,
    tr1: RollingWindow<Decimal>, // true range
    tr2: RollingWindow<Decimal>,
    tr3: RollingWindow<Decimal>,
    samples: usize,
    current: IndicatorResult,
}

impl UltimateOscillator {
    pub fn new(period1: usize, period2: usize, period3: usize) -> Self {
        let max_period = period1.max(period2).max(period3);
        UltimateOscillator {
            name: format!("ULTOSC({},{},{})", period1, period2, period3),
            max_period,
            prev_close: None,
            bp1: RollingWindow::new(period1),
            bp2: RollingWindow::new(period2),
            bp3: RollingWindow::new(period3),
            tr1: RollingWindow::new(period1),
            tr2: RollingWindow::new(period2),
            tr3: RollingWindow::new(period3),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    fn sum_window(w: &RollingWindow<Decimal>) -> Decimal {
        w.iter().copied().sum()
    }
}

impl Default for UltimateOscillator {
    fn default() -> Self {
        Self::new(7, 14, 28)
    }
}

impl Indicator for UltimateOscillator {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > self.max_period
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.max_period + 1
    }

    fn reset(&mut self) {
        self.prev_close = None;
        self.bp1.clear();
        self.bp2.clear();
        self.bp3.clear();
        self.tr1.clear();
        self.tr2.clear();
        self.tr3.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;

        let prev_close = self.prev_close.unwrap_or(bar.close);
        let hl = bar.high - bar.low;
        let hc = (bar.high - prev_close).abs();
        let lc = (bar.low - prev_close).abs();
        let tr = hl.max(hc).max(lc);
        let bp = bar.close - bar.low.min(prev_close);

        self.bp1.push(bp);
        self.bp2.push(bp);
        self.bp3.push(bp);
        self.tr1.push(tr);
        self.tr2.push(tr);
        self.tr3.push(tr);
        self.prev_close = Some(bar.close);

        if self.is_ready() {
            let s1_tr = Self::sum_window(&self.tr1);
            let s2_tr = Self::sum_window(&self.tr2);
            let s3_tr = Self::sum_window(&self.tr3);

            if s1_tr == dec!(0) || s2_tr == dec!(0) || s3_tr == dec!(0) {
                return self.current.clone();
            }

            let avg1 = Self::sum_window(&self.bp1) / s1_tr;
            let avg2 = Self::sum_window(&self.bp2) / s2_tr;
            let avg3 = Self::sum_window(&self.bp3) / s3_tr;

            let v = dec!(100) * (dec!(4) * avg1 + dec!(2) * avg2 + avg3) / dec!(7);
            self.current = IndicatorResult::ready(v, bar.time);
        }

        self.current.clone()
    }
}
