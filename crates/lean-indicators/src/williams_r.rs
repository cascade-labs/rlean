use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

pub struct WilliamsR {
    name: String,
    period: usize,
    window: RollingWindow<(Price, Price, Price)>,
    samples: usize,
    current: IndicatorResult,
}

impl WilliamsR {
    pub fn new(period: usize) -> Self {
        WilliamsR {
            name: format!("WILLR({})", period),
            period,
            window: RollingWindow::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for WilliamsR {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.window.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }
    fn reset(&mut self) { self.window.clear(); self.samples = 0; self.current = IndicatorResult::not_ready(); }
    fn update_price(&mut self, _: DateTime, _: Price) -> IndicatorResult { self.current.clone() }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        self.window.push((bar.high, bar.low, bar.close));

        if self.window.is_full() {
            let hh = self.window.iter().map(|(h,_,_)| *h).fold(dec!(0), |a,x| a.max(x));
            let ll = self.window.iter().map(|(_,l,_)| *l).fold(Price::MAX, |a,x| a.min(x));
            let r = if (hh - ll).is_zero() { dec!(-50) } else {
                dec!(-100) * (hh - bar.close) / (hh - ll)
            };
            self.current = IndicatorResult::ready(r, bar.time);
        }

        self.current.clone()
    }
}
