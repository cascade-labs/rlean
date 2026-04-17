use crate::{
    indicator::{Indicator, IndicatorResult},
    rsi::Rsi,
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Connors RSI. (RSI(3) + RSI(streak,2) + PercentRank(100)) / 3
pub struct ConnorsRsi {
    name: String,
    rsi: Rsi,
    streak_rsi: Rsi,
    price_changes: RollingWindow<Decimal>,
    trend_streak: i64,
    prev_value: Option<Decimal>,
    warm_up: usize,
    samples: usize,
    current: IndicatorResult,
}

impl ConnorsRsi {
    pub fn new(rsi_period: usize, streak_period: usize, rank_period: usize) -> Self {
        let warm_up = rank_period.max(rsi_period).max(streak_period);
        ConnorsRsi {
            name: format!("CRSI({},{},{})", rsi_period, streak_period, rank_period),
            rsi: Rsi::new(rsi_period),
            streak_rsi: Rsi::new(streak_period),
            price_changes: RollingWindow::new(rank_period),
            trend_streak: 0,
            prev_value: None,
            warm_up,
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Default for ConnorsRsi {
    fn default() -> Self {
        Self::new(3, 2, 100)
    }
}

impl Indicator for ConnorsRsi {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.rsi.is_ready() && self.streak_rsi.is_ready() && self.price_changes.is_full()
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.warm_up
    }

    fn reset(&mut self) {
        self.rsi.reset();
        self.streak_rsi.reset();
        self.price_changes.clear();
        self.trend_streak = 0;
        self.prev_value = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult {
        self.samples += 1;
        self.rsi.update_price(time, value);

        // Compute trend streak
        if let Some(prev) = self.prev_value {
            let change = value - prev;
            if (self.trend_streak > 0 && change < dec!(0))
                || (self.trend_streak < 0 && change > dec!(0))
            {
                self.trend_streak = 0;
            }
            if change > dec!(0) {
                self.trend_streak += 1;
            } else if change < dec!(0) {
                self.trend_streak -= 1;
            }
        }

        self.streak_rsi
            .update_price(time, Decimal::from(self.trend_streak));

        // PercentRank
        let pct_rank = match self.prev_value {
            Some(prev) if prev != dec!(0) => {
                let ratio = (value - prev) / prev;

                let rank = if self.price_changes.is_full() {
                    let count = self.price_changes.len();
                    let below = self.price_changes.iter().filter(|&&x| x < ratio).count();
                    dec!(100) * Decimal::from(below) / Decimal::from(count)
                } else {
                    dec!(0)
                };

                self.price_changes.push(ratio);
                rank
            }
            _ => {
                self.price_changes.push(dec!(0));
                dec!(0)
            }
        };

        self.prev_value = Some(value);

        if self.is_ready() {
            let v =
                (self.rsi.current().value + self.streak_rsi.current().value + pct_rank) / dec!(3);
            self.current = IndicatorResult::ready(v, time);
        }

        self.current.clone()
    }
}
