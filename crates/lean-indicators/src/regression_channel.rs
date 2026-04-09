use crate::{indicator::{Indicator, IndicatorResult}, lsma::Lsma, standard_deviation::StandardDeviation};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone)]
pub struct RegressionChannelResult {
    pub middle: Decimal,
    pub upper: Decimal,
    pub lower: Decimal,
}

/// Regression Channel. LSMA +/- k*StdDev.
pub struct RegressionChannel {
    name: String,
    period: usize,
    k: Decimal,
    lsma: Lsma,
    std: StandardDeviation,
    samples: usize,
    current: IndicatorResult,
    pub last_result: RegressionChannelResult,
}

impl RegressionChannel {
    pub fn new(period: usize, k: Decimal) -> Self {
        RegressionChannel {
            name: format!("RC({},{})", period, k),
            period,
            k,
            lsma: Lsma::new(period),
            std: StandardDeviation::new(period),
            samples: 0,
            current: IndicatorResult::not_ready(),
            last_result: RegressionChannelResult { middle: dec!(0), upper: dec!(0), lower: dec!(0) },
        }
    }
}

impl Indicator for RegressionChannel {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.lsma.is_ready() && self.std.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.lsma.reset();
        self.std.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        let rl = self.lsma.update_price(time, value);
        let rs = self.std.update_price(time, value);

        if rl.is_ready() && rs.is_ready() {
            let mid = rl.value;
            let upper = mid + self.k * rs.value;
            let lower = mid - self.k * rs.value;
            self.last_result = RegressionChannelResult { middle: mid, upper, lower };
            self.current = IndicatorResult::ready(mid, time);
        }

        self.current.clone()
    }
}
