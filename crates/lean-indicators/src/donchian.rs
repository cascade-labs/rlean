use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

pub struct DonchianChannel {
    name: String,
    period: usize,
    window: RollingWindow<(Price, Price)>,
    samples: usize,
    pub upper: Price,
    pub lower: Price,
    pub middle: Price,
    current: IndicatorResult,
}

impl DonchianChannel {
    pub fn new(period: usize) -> Self {
        DonchianChannel {
            name: format!("DC({})", period),
            period,
            window: RollingWindow::new(period),
            samples: 0,
            upper: dec!(0),
            lower: dec!(0),
            middle: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for DonchianChannel {
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
        self.period
    }
    fn reset(&mut self) {
        self.window.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }
    fn update_price(&mut self, _: DateTime, _: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.samples += 1;
        self.window.push((bar.high, bar.low));

        if self.window.is_full() {
            self.upper = self
                .window
                .iter()
                .map(|(h, _)| *h)
                .fold(dec!(0), |a, x| a.max(x));
            self.lower = self
                .window
                .iter()
                .map(|(_, l)| *l)
                .fold(Price::MAX, |a, x| a.min(x));
            self.middle = (self.upper + self.lower) / dec!(2);
            self.current = IndicatorResult::ready(self.middle, bar.time);
        }

        self.current.clone()
    }
}
