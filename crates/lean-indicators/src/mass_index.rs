use crate::{indicator::{Indicator, IndicatorResult}, ema::Ema, window::RollingWindow};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Mass Index. sum(EMA(H-L,9) / EMA(EMA(H-L,9),9), sum_period).
pub struct MassIndex {
    name: String,
    ema1: Ema,
    ema2: Ema,
    sum_window: RollingWindow<Decimal>,
    running_sum: Decimal,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl MassIndex {
    pub fn new(ema_period: usize, sum_period: usize) -> Self {
        MassIndex {
            name: format!("MASS({},{})", ema_period, sum_period),
            ema1: Ema::new(ema_period),
            ema2: Ema::new(ema_period),
            sum_window: RollingWindow::new(sum_period),
            running_sum: dec!(0),
            warm_up: 2 * (ema_period - 1) + sum_period,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default() -> Self {
        Self::new(9, 25)
    }
}

impl Indicator for MassIndex {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.sum_window.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.ema1.reset();
        self.ema2.reset();
        self.sum_window.clear();
        self.running_sum = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let hl = bar.high - bar.low;
        let r1 = self.ema1.update_price(bar.time, hl);

        if r1.is_ready() {
            let r2 = self.ema2.update_price(bar.time, r1.value);
            if r2.is_ready() {
                let ratio = if r2.value != dec!(0) { r1.value / r2.value } else { dec!(0) };

                if self.sum_window.is_full() {
                    if let Some(oldest) = self.sum_window.oldest() {
                        self.running_sum -= *oldest;
                    }
                }
                self.sum_window.push(ratio);
                self.running_sum += ratio;

                if self.is_ready() {
                    self.current = IndicatorResult::ready(self.running_sum, bar.time);
                }
            }
        }

        self.current.clone()
    }
}
