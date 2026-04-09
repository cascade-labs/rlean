use crate::{indicator::{Indicator, IndicatorResult}, wma::Wma};
use lean_core::{DateTime, Price};
use rust_decimal_macros::dec;

/// Hull Moving Average. HMA = WMA(2*WMA(n/2) - WMA(n), sqrt(n)).
pub struct Hma {
    name: String,
    period: usize,
    fast_wma: Wma,
    slow_wma: Wma,
    hull_wma: Wma,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl Hma {
    pub fn new(period: usize) -> Self {
        assert!(period >= 2, "HMA period must be >= 2");
        let fast_period = (period as f64 / 2.0).round() as usize;
        let k = (period as f64).sqrt().round() as usize;
        let warm_up = period + k - 1;
        Hma {
            name: format!("HMA({})", period),
            period,
            fast_wma: Wma::new(fast_period),
            slow_wma: Wma::new(period),
            hull_wma: Wma::new(k),
            warm_up,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Hma {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.hull_wma.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.fast_wma.reset();
        self.slow_wma.reset();
        self.hull_wma.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let rf = self.fast_wma.update_price(time, value);
        let rs = self.slow_wma.update_price(time, value);

        if rf.is_ready() && rs.is_ready() {
            let hull_input = dec!(2) * rf.value - rs.value;
            let rh = self.hull_wma.update_price(time, hull_input);
            if rh.is_ready() {
                self.current = IndicatorResult::ready(rh.value, time);
            }
        }

        self.current.clone()
    }
}
