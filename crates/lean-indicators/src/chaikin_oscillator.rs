use crate::{
    accumulation_distribution::AccumulationDistribution,
    ema::Ema,
    indicator::{Indicator, IndicatorResult},
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;

/// Chaikin Oscillator. fast EMA(AD) - slow EMA(AD).
pub struct ChaikinOscillator {
    name: String,
    ad: AccumulationDistribution,
    fast_ema: Ema,
    slow_ema: Ema,
    slow_period: usize,
    samples: usize,
    current: IndicatorResult,
}

impl ChaikinOscillator {
    pub fn new(fast_period: usize, slow_period: usize) -> Self {
        ChaikinOscillator {
            name: format!("ChaikinOscillator({},{})", fast_period, slow_period),
            ad: AccumulationDistribution::new(),
            fast_ema: Ema::new(fast_period),
            slow_ema: Ema::new(slow_period),
            slow_period,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for ChaikinOscillator {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= self.slow_period
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.slow_period
    }

    fn reset(&mut self) {
        self.ad.reset();
        self.fast_ema.reset();
        self.slow_ema.reset();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let r_ad = self.ad.update_bar(bar);
        self.fast_ema.update_price(bar.time, r_ad.value);
        self.slow_ema.update_price(bar.time, r_ad.value);

        if self.is_ready() {
            let v = self.fast_ema.current().value - self.slow_ema.current().value;
            self.current = IndicatorResult::ready(v, bar.time);
        }

        self.current.clone()
    }
}
