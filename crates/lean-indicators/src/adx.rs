use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Average Directional Index (Wilder).
pub struct Adx {
    name: String,
    period: usize,
    prev_high: Option<Price>,
    prev_low: Option<Price>,
    prev_close: Option<Price>,
    smoothed_tr: Price,
    smoothed_plus_dm: Price,
    smoothed_minus_dm: Price,
    dx_sum: Price,
    samples: usize,
    adx: Price,
    pub plus_di: Price,
    pub minus_di: Price,
    current: IndicatorResult,
}

impl Adx {
    pub fn new(period: usize) -> Self {
        Adx {
            name: format!("ADX({})", period),
            period,
            prev_high: None,
            prev_low: None,
            prev_close: None,
            smoothed_tr: dec!(0),
            smoothed_plus_dm: dec!(0),
            smoothed_minus_dm: dec!(0),
            dx_sum: dec!(0),
            samples: 0,
            adx: dec!(0),
            plus_di: dec!(0),
            minus_di: dec!(0),
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for Adx {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= self.period * 2
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.period * 2
    }

    fn reset(&mut self) {
        self.prev_high = None;
        self.prev_low = None;
        self.prev_close = None;
        self.smoothed_tr = dec!(0);
        self.smoothed_plus_dm = dec!(0);
        self.smoothed_minus_dm = dec!(0);
        self.dx_sum = dec!(0);
        self.samples = 0;
        self.adx = dec!(0);
        self.plus_di = dec!(0);
        self.minus_di = dec!(0);
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        if let (Some(ph), Some(pl), Some(pc)) = (self.prev_high, self.prev_low, self.prev_close) {
            self.samples += 1;

            let up_move = bar.high - ph;
            let down_move = pl - bar.low;

            let plus_dm = if up_move > down_move && up_move > dec!(0) {
                up_move
            } else {
                dec!(0)
            };
            let minus_dm = if down_move > up_move && down_move > dec!(0) {
                down_move
            } else {
                dec!(0)
            };

            let tr = (bar.high - bar.low)
                .max((bar.high - pc).abs())
                .max((bar.low - pc).abs());

            let n = Decimal::from(self.period);

            if self.samples == 1 {
                self.smoothed_tr = tr;
                self.smoothed_plus_dm = plus_dm;
                self.smoothed_minus_dm = minus_dm;
            } else {
                // Wilder smoothing
                self.smoothed_tr = self.smoothed_tr - (self.smoothed_tr / n) + tr;
                self.smoothed_plus_dm =
                    self.smoothed_plus_dm - (self.smoothed_plus_dm / n) + plus_dm;
                self.smoothed_minus_dm =
                    self.smoothed_minus_dm - (self.smoothed_minus_dm / n) + minus_dm;
            }

            if !self.smoothed_tr.is_zero() {
                self.plus_di = dec!(100) * self.smoothed_plus_dm / self.smoothed_tr;
                self.minus_di = dec!(100) * self.smoothed_minus_dm / self.smoothed_tr;
            }

            let di_sum = self.plus_di + self.minus_di;
            let dx = if di_sum.is_zero() {
                dec!(0)
            } else {
                dec!(100) * (self.plus_di - self.minus_di).abs() / di_sum
            };

            if self.samples <= self.period {
                self.dx_sum += dx;
                if self.samples == self.period {
                    self.adx = self.dx_sum / n;
                }
            } else {
                self.adx = (self.adx * (n - dec!(1)) + dx) / n;
                self.current = IndicatorResult::ready(self.adx, bar.time);
            }
        }

        self.prev_high = Some(bar.high);
        self.prev_low = Some(bar.low);
        self.prev_close = Some(bar.close);

        self.current.clone()
    }
}
