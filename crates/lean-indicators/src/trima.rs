use crate::{
    indicator::{Indicator, IndicatorResult},
    sma::Sma,
};
use lean_core::{DateTime, Price};

/// Triangular Moving Average. SMA of SMA.
/// Even period: TRIMA(x,n) = SMA(SMA(x,n/2), n/2+1)
/// Odd period:  TRIMA(x,n) = SMA(SMA(x,(n+1)/2), (n+1)/2)
pub struct Trima {
    name: String,
    period: usize,
    sma1: Sma,
    sma2: Sma,
    samples: usize,
    current: IndicatorResult,
}

impl Trima {
    pub fn new(period: usize) -> Self {
        let half = period.div_ceil(2);
        let (p1, p2) = if period.is_multiple_of(2) {
            (period / 2, period / 2 + 1)
        } else {
            (half, half)
        };
        Trima {
            name: format!("TRIMA({})", period),
            period,
            sma1: Sma::new(p1),
            sma2: Sma::new(p2),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Trima {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= self.period
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
        self.sma1.reset();
        self.sma2.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let r1 = self.sma1.update_price(time, value);

        if r1.is_ready() {
            let r2 = self.sma2.update_price(time, r1.value);
            if r2.is_ready() && self.is_ready() {
                self.current = IndicatorResult::ready(r2.value, time);
            }
        }

        self.current.clone()
    }
}
