use crate::{indicator::{Indicator, IndicatorResult}, sma::Sma, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Bollinger Bands — middle (SMA), upper = mid + k*std, lower = mid - k*std.
pub struct BollingerBands {
    name: String,
    period: usize,
    k: Decimal,
    sma: Sma,
    window: RollingWindow<Price>,
    samples: usize,
    pub middle: Price,
    pub upper: Price,
    pub lower: Price,
    pub bandwidth: Price,
    pub percent_b: Price,
    current: IndicatorResult,
}

impl BollingerBands {
    pub fn new(period: usize, k: Decimal) -> Self {
        BollingerBands {
            name: format!("BB({},{})", period, k),
            period,
            k,
            sma: Sma::new(period),
            window: RollingWindow::new(period),
            samples: 0,
            middle: dec!(0),
            upper: dec!(0),
            lower: dec!(0),
            bandwidth: dec!(0),
            percent_b: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn standard(period: usize) -> Self {
        Self::new(period, dec!(2))
    }

    fn std_dev(&self, mean: Price) -> Price {
        let n = Decimal::from(self.period);
        let variance: Price = self.window.iter()
            .map(|&x| (x - mean) * (x - mean))
            .sum::<Price>() / n;

        // Integer square root approximation via Newton's method on Decimal
        use rust_decimal::prelude::ToPrimitive;
        let v_f64 = variance.to_f64().unwrap_or(0.0);
        Decimal::from_f64_retain(v_f64.sqrt()).unwrap_or(dec!(0))
    }
}

impl Indicator for BollingerBands {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.window.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.sma.reset();
        self.window.clear();
        self.samples = 0;
        self.middle = dec!(0);
        self.upper = dec!(0);
        self.lower = dec!(0);
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.window.push(value);
        self.sma.update_price(time, value);
        self.samples += 1;

        if self.is_ready() {
            let mid = self.sma.current().value;
            let std = self.std_dev(mid);
            let band = self.k * std;

            self.middle = mid;
            self.upper = mid + band;
            self.lower = mid - band;

            let range = self.upper - self.lower;
            self.bandwidth = if mid.is_zero() { dec!(0) } else { range / mid };
            self.percent_b = if range.is_zero() { dec!(0.5) } else {
                (value - self.lower) / range
            };

            self.current = IndicatorResult::ready(mid, time);
        }

        self.current.clone()
    }
}
