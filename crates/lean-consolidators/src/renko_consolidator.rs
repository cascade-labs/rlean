use lean_core::{DateTime, Symbol, TimeSpan};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::consolidator::IConsolidator;

/// Renko ("Wicked" style) consolidator.
///
/// Each Renko brick represents a fixed price move of `brick_size`.
/// Mirrors C# RenkoConsolidator (Wicked variant).
///
/// In Wicked Renko:
/// - A rising brick: open = prev_open, close = prev_open + brick_size, shadows extend to real H/L
/// - A falling brick: open = prev_open, close = prev_open - brick_size, shadows extend to real H/L
/// - Reversals are handled by checking against the last emitted brick's direction.
pub struct RenkoConsolidator {
    brick_size: Decimal,
    first_tick: bool,
    /// Current open time of the working brick.
    open_on: Option<DateTime>,
    /// Current close time (updated with every tick).
    close_on: Option<DateTime>,
    open_rate: Decimal,
    close_rate: Decimal,
    high_rate: Decimal,
    low_rate: Decimal,
    /// Direction and open of the last emitted brick (used for reversal logic).
    last_brick_open: Option<Decimal>,
    last_brick_direction: Option<i8>, // 1 = rising, -1 = falling
    /// Symbol from the first bar seen.
    symbol: Option<Symbol>,
    /// Pending consolidated bars (may emit multiple per update).
    pending: Vec<TradeBar>,
}

impl RenkoConsolidator {
    pub fn new(brick_size: Decimal) -> Self {
        assert!(brick_size > dec!(0), "brick_size must be > 0");
        Self {
            brick_size,
            first_tick: true,
            open_on: None,
            close_on: None,
            open_rate: dec!(0),
            close_rate: dec!(0),
            high_rate: dec!(0),
            low_rate: dec!(0),
            last_brick_open: None,
            last_brick_direction: None,
            symbol: None,
            pending: Vec::new(),
        }
    }

    /// Rounds price to nearest brick_size multiple (Knuth's algorithm).
    pub fn get_closest_multiple(price: Decimal, brick_size: Decimal) -> Decimal {
        let floor_div = (price / brick_size).floor();
        let modulus = price - brick_size * floor_div;
        let round = (modulus / brick_size).round();
        brick_size * (floor_div + round)
    }

    fn emit_rising(&mut self, data_time: DateTime, symbol: &Symbol) {
        let limit = self.open_rate + self.brick_size;
        while self.close_rate > limit {
            let limit = self.open_rate + self.brick_size;
            if self.close_rate <= limit {
                break;
            }
            let bar = self.make_bar(
                symbol,
                self.open_on.unwrap_or(data_time),
                self.close_on.unwrap_or(data_time),
                self.open_rate,
                limit,        // close at limit
                self.low_rate,
                limit,        // high at limit (wicked)
            );
            self.last_brick_open = Some(self.open_rate);
            self.last_brick_direction = Some(1);
            self.pending.push(bar);
            // Advance
            self.open_on = self.close_on;
            self.open_rate = limit;
            self.low_rate = limit;
        }
    }

    fn emit_falling(&mut self, data_time: DateTime, symbol: &Symbol) {
        let limit = self.open_rate - self.brick_size;
        while self.close_rate < limit {
            let limit = self.open_rate - self.brick_size;
            if self.close_rate >= limit {
                break;
            }
            let bar = self.make_bar(
                symbol,
                self.open_on.unwrap_or(data_time),
                self.close_on.unwrap_or(data_time),
                self.open_rate,
                limit,        // close at limit (down)
                limit,        // low at limit (wicked)
                self.high_rate,
            );
            self.last_brick_open = Some(self.open_rate);
            self.last_brick_direction = Some(-1);
            self.pending.push(bar);
            // Advance
            self.open_on = self.close_on;
            self.open_rate = limit;
            self.high_rate = limit;
        }
    }

    /// Build a synthetic TradeBar for one Renko brick.
    /// open/close are the brick price boundaries; high/low are the wicked extremes.
    fn make_bar(
        &self,
        symbol: &Symbol,
        open_on: DateTime,
        close_on: DateTime,
        open_price: Decimal,
        close_price: Decimal,
        low_price: Decimal,
        high_price: Decimal,
    ) -> TradeBar {
        let period_nanos = (close_on.0 - open_on.0).max(0);
        TradeBar {
            symbol: symbol.clone(),
            time: open_on,
            end_time: close_on,
            open: open_price,
            high: high_price,
            low: low_price,
            close: close_price,
            volume: dec!(0),
            period: TimeSpan::from_nanos(period_nanos),
        }
    }
}

impl IConsolidator for RenkoConsolidator {
    /// Returns the first consolidated brick if one was produced, or None.
    /// Callers should drain all bricks by calling `pop_pending` or by noting
    /// that a single `update` call may produce multiple bricks (stored internally).
    /// For convenience the trait returns the *first* pending brick; see `drain()`.
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        let rate = bar.close;
        let symbol = &bar.symbol;

        if self.symbol.is_none() {
            self.symbol = Some(symbol.clone());
        }

        if self.first_tick {
            self.first_tick = false;
            let rounded = Self::get_closest_multiple(rate, self.brick_size);
            self.open_on = Some(bar.time);
            self.close_on = Some(bar.time);
            self.open_rate = rounded;
            self.high_rate = rounded;
            self.low_rate = rounded;
            self.close_rate = rounded;
            return None;
        }

        self.close_on = Some(bar.time);
        if rate > self.high_rate { self.high_rate = rate; }
        if rate < self.low_rate  { self.low_rate  = rate; }
        self.close_rate = rate;

        if self.close_rate > self.open_rate {
            match self.last_brick_direction {
                None | Some(1) => {
                    // Continuing upward trend
                    self.emit_rising(bar.time, symbol);
                }
                Some(-1) => {
                    // Was falling — check for reversal
                    let last_open = self.last_brick_open.unwrap_or(self.open_rate);
                    let limit = last_open + self.brick_size;
                    if self.close_rate > limit {
                        let reversal_bar = self.make_bar(
                            symbol,
                            self.open_on.unwrap_or(bar.time),
                            self.close_on.unwrap_or(bar.time),
                            last_open,
                            limit,
                            self.low_rate,
                            limit,
                        );
                        self.last_brick_open = Some(last_open);
                        self.last_brick_direction = Some(1);
                        self.pending.push(reversal_bar);
                        self.open_on = self.close_on;
                        self.open_rate = limit;
                        self.low_rate = limit;
                        self.emit_rising(bar.time, symbol);
                    }
                }
                _ => {}
            }
        } else if self.close_rate < self.open_rate {
            match self.last_brick_direction {
                None | Some(-1) => {
                    // Continuing downward trend
                    self.emit_falling(bar.time, symbol);
                }
                Some(1) => {
                    // Was rising — check for reversal
                    let last_open = self.last_brick_open.unwrap_or(self.open_rate);
                    let limit = last_open - self.brick_size;
                    if self.close_rate < limit {
                        let reversal_bar = self.make_bar(
                            symbol,
                            self.open_on.unwrap_or(bar.time),
                            self.close_on.unwrap_or(bar.time),
                            last_open,
                            limit,
                            limit,
                            self.high_rate,
                        );
                        self.last_brick_open = Some(last_open);
                        self.last_brick_direction = Some(-1);
                        self.pending.push(reversal_bar);
                        self.open_on = self.close_on;
                        self.open_rate = limit;
                        self.high_rate = limit;
                        self.emit_falling(bar.time, symbol);
                    }
                }
                _ => {}
            }
        }

        if self.pending.is_empty() {
            None
        } else {
            Some(self.pending.remove(0))
        }
    }

    fn reset(&mut self) {
        self.first_tick = true;
        self.open_on = None;
        self.close_on = None;
        self.open_rate = dec!(0);
        self.close_rate = dec!(0);
        self.high_rate = dec!(0);
        self.low_rate = dec!(0);
        self.last_brick_open = None;
        self.last_brick_direction = None;
        self.symbol = None;
        self.pending.clear();
    }

    fn name(&self) -> &str {
        "RenkoConsolidator"
    }
}

impl RenkoConsolidator {
    /// Drain all pending bricks produced by the last `update()` call.
    pub fn drain_pending(&mut self) -> Vec<TradeBar> {
        std::mem::take(&mut self.pending)
    }
}
