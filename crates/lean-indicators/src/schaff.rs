use crate::{indicator::{Indicator, IndicatorResult}, ema::Ema, window::RollingWindow};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Schaff Trend Cycle. Stochastic of MACD, double-smoothed.
pub struct SchaffTrendCycle {
    name: String,
    // MACD components
    fast_ema: Ema,
    slow_ema: Ema,
    // cycle_period stochastic windows of MACD
    macd_win: RollingWindow<Decimal>,
    // K smoothing (EMA 3)
    k_ema: Ema,
    // Second layer
    d_win: RollingWindow<Decimal>,
    pf_ema: Ema,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl SchaffTrendCycle {
    pub fn new(cycle_period: usize, fast_period: usize, slow_period: usize) -> Self {
        let warm_up = slow_period;
        SchaffTrendCycle {
            name: format!("STC({},{},{})", cycle_period, fast_period, slow_period),
            fast_ema: Ema::new(fast_period),
            slow_ema: Ema::new(slow_period),
            macd_win: RollingWindow::new(cycle_period),
            k_ema: Ema::new(3),
            d_win: RollingWindow::new(cycle_period),
            pf_ema: Ema::new(3),
            warm_up,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default() -> Self {
        Self::new(10, 23, 50)
    }

    fn stoch(value: Decimal, highest: Decimal, lowest: Decimal) -> Decimal {
        let denom = highest - lowest;
        if denom > dec!(0) { (value - lowest) / denom * dec!(100) } else { dec!(0) }
    }
}

impl Indicator for SchaffTrendCycle {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.fast_ema.is_ready() && self.slow_ema.is_ready() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.fast_ema.reset();
        self.slow_ema.reset();
        self.macd_win.clear();
        self.k_ema.reset();
        self.d_win.clear();
        self.pf_ema.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.fast_ema.update_price(time, value);
        self.slow_ema.update_price(time, value);

        if !self.is_ready() {
            return self.current.clone();
        }

        let macd = self.fast_ema.current().value - self.slow_ema.current().value;
        self.macd_win.push(macd);

        let max_macd = self.macd_win.iter().copied().fold(Decimal::MIN, Decimal::max);
        let min_macd = self.macd_win.iter().copied().fold(Decimal::MAX, Decimal::min);
        let k_raw = Self::stoch(macd, max_macd, min_macd);
        let k = self.k_ema.update_price(time, k_raw).value;
        self.d_win.push(k);

        let max_d = self.d_win.iter().copied().fold(Decimal::MIN, Decimal::max);
        let min_d = self.d_win.iter().copied().fold(Decimal::MAX, Decimal::min);
        let pf_raw = Self::stoch(k, max_d, min_d);
        let pff = self.pf_ema.update_price(time, pf_raw).value;

        self.current = IndicatorResult::ready(pff, time);
        self.current.clone()
    }
}
