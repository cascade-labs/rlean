use crate::indicator::{Indicator, IndicatorResult};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Relative Strength Index (Wilder smoothing).
pub struct Rsi {
    name: String,
    period: usize,
    avg_gain: Price,
    avg_loss: Price,
    prev_value: Option<Price>,
    samples: usize,
    current: IndicatorResult,
    // Accumulate initial gains/losses for the seed SMA
    initial_gains: Vec<Price>,
    initial_losses: Vec<Price>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        Rsi {
            name: format!("RSI({})", period),
            period,
            avg_gain: dec!(0),
            avg_loss: dec!(0),
            prev_value: None,
            samples: 0,
            current: IndicatorResult::not_ready(),
            initial_gains: Vec::with_capacity(period),
            initial_losses: Vec::with_capacity(period),
        }
    }

    pub fn is_overbought(&self) -> bool {
        self.is_ready() && self.current.value >= dec!(70)
    }

    pub fn is_oversold(&self) -> bool {
        self.is_ready() && self.current.value <= dec!(30)
    }
}

impl Indicator for Rsi {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples > self.period
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
        self.avg_gain = dec!(0);
        self.avg_loss = dec!(0);
        self.prev_value = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
        self.initial_gains.clear();
        self.initial_losses.clear();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;

        if let Some(prev) = self.prev_value {
            let change = value - prev;
            let gain = if change > dec!(0) { change } else { dec!(0) };
            let loss = if change < dec!(0) { -change } else { dec!(0) };

            if self.samples <= self.period {
                // Collecting initial period
                self.initial_gains.push(gain);
                self.initial_losses.push(loss);

                if self.samples == self.period {
                    // Seed: simple average of first period
                    let n = Decimal::from(self.period);
                    self.avg_gain = self.initial_gains.iter().sum::<Price>() / n;
                    self.avg_loss = self.initial_losses.iter().sum::<Price>() / n;
                }
            } else {
                // Wilder smoothing: avg = (prev_avg * (period-1) + current) / period
                let n = Decimal::from(self.period);
                self.avg_gain = (self.avg_gain * (n - dec!(1)) + gain) / n;
                self.avg_loss = (self.avg_loss * (n - dec!(1)) + loss) / n;

                let rsi = if self.avg_loss.is_zero() {
                    dec!(100)
                } else {
                    let rs = self.avg_gain / self.avg_loss;
                    dec!(100) - (dec!(100) / (dec!(1) + rs))
                };

                self.current = IndicatorResult::ready(rsi, time);
            }
        }

        self.prev_value = Some(value);
        self.current.clone()
    }
}
