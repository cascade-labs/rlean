use crate::{indicator::{Indicator, IndicatorResult}, ema::Ema};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// True Strength Index (TSI).
/// TSI = 100 * double_smoothed_momentum / double_smoothed_abs_momentum
pub struct Tsi {
    name: String,
    long_period: usize,
    short_period: usize,
    ema_pc: Ema,       // EMA of price change
    ema_pc_pc: Ema,    // EMA of EMA of price change
    ema_apc: Ema,      // EMA of |price change|
    ema_apc_pc: Ema,   // EMA of EMA of |price change|
    prev_close: Option<Decimal>,
    samples: usize,
    warm_up: usize,
    current: IndicatorResult,
}

impl Tsi {
    pub fn new(long_period: usize, short_period: usize) -> Self {
        Tsi {
            name: format!("TSI({},{})", long_period, short_period),
            long_period,
            short_period,
            ema_pc: Ema::new(long_period),
            ema_pc_pc: Ema::new(short_period),
            ema_apc: Ema::new(long_period),
            ema_apc_pc: Ema::new(short_period),
            prev_close: None,
            samples: 0,
            warm_up: long_period + short_period,
            current: IndicatorResult::not_ready(),
        }
    }

    pub fn default() -> Self {
        Self::new(25, 13)
    }
}

impl Indicator for Tsi {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.samples >= self.warm_up }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.warm_up }

    fn reset(&mut self) {
        self.ema_pc.reset();
        self.ema_pc_pc.reset();
        self.ema_apc.reset();
        self.ema_apc_pc.reset();
        self.prev_close = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if self.samples == 1 {
            self.prev_close = Some(value);
            return self.current.clone();
        }

        let prev = self.prev_close.unwrap_or(value);
        let pc = value - prev;
        let apc = pc.abs();
        self.prev_close = Some(value);

        let r_pc = self.ema_pc.update_price(time, pc);
        self.ema_apc.update_price(time, apc);

        if r_pc.is_ready() {
            let r_pc2 = self.ema_pc_pc.update_price(time, r_pc.value);
            let r_apc = self.ema_apc.current();
            self.ema_apc_pc.update_price(time, r_apc.value);

            if r_pc2.is_ready() {
                let apc2 = self.ema_apc_pc.current().value;
                let v = if apc2 != dec!(0) {
                    dec!(100) * r_pc2.value / apc2
                } else {
                    dec!(0)
                };
                if self.is_ready() {
                    self.current = IndicatorResult::ready(v, time);
                }
            }
        }

        self.current.clone()
    }
}
