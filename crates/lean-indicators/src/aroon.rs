use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub struct Aroon {
    name: String,
    period: usize,
    window: RollingWindow<(Price, Price)>, // (high, low)
    samples: usize,
    pub up: Price,
    pub down: Price,
    current: IndicatorResult,
}

impl Aroon {
    pub fn new(period: usize) -> Self {
        Aroon {
            name: format!("Aroon({})", period),
            period,
            window: RollingWindow::new(period + 1),
            samples: 0,
            up: dec!(0),
            down: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Aroon {
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
            let n = Decimal::from(self.period);
            // Find bars since highest high and lowest low
            let bars: Vec<_> = self.window.iter().collect();
            let max_high = bars
                .iter()
                .map(|(h, _)| h)
                .cloned()
                .fold(dec!(0), |a, x| a.max(x));
            let min_low = bars
                .iter()
                .map(|(_, l)| l)
                .cloned()
                .fold(Price::MAX, |a, x| a.min(x));

            let periods_since_high = bars.iter().position(|(h, _)| *h == max_high).unwrap_or(0);
            let periods_since_low = bars.iter().position(|(_, l)| *l == min_low).unwrap_or(0);

            self.up = dec!(100) * (n - Decimal::from(periods_since_high)) / n;
            self.down = dec!(100) * (n - Decimal::from(periods_since_low)) / n;

            self.current = IndicatorResult::ready(self.up - self.down, bar.time);
        }

        self.current.clone()
    }
}
