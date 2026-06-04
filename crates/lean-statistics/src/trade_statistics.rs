use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub symbol: Symbol,
    pub entry_time: DateTime,
    pub exit_time: DateTime,
    pub entry_price: Price,
    pub exit_price: Price,
    pub quantity: Quantity,
    pub pnl: Price,
    pub pnl_pct: Price,
    pub fees: Price,
    pub is_win: bool,
}

impl Trade {
    pub fn new(
        symbol: Symbol,
        entry_time: DateTime,
        exit_time: DateTime,
        entry_price: Price,
        exit_price: Price,
        quantity: Quantity,
        fees: Price,
    ) -> Self {
        let pnl = (exit_price - entry_price) * quantity - fees;
        let pnl_pct = if entry_price.is_zero() {
            dec!(0)
        } else if quantity < dec!(0) {
            (entry_price - exit_price) / entry_price
        } else {
            (exit_price - entry_price) / entry_price
        };
        Trade {
            symbol,
            entry_time,
            exit_time,
            entry_price,
            exit_price,
            quantity,
            pnl,
            pnl_pct,
            fees,
            is_win: pnl > dec!(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenPositionTrade {
    pub entry_time: DateTime,
    pub entry_price: Price,
    pub quantity: Quantity,
    pub fees: Price,
}

/// Builds completed round-trip trades from a stream of fills.
///
/// The builder keeps the open position state needed by statistics code and
/// emits a `Trade` whenever a fill closes all or part of an existing position.
#[derive(Debug, Clone, Default)]
pub struct TradeBuilder {
    open_positions: HashMap<u64, OpenPositionTrade>,
}

impl TradeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_position(&self, symbol: &Symbol) -> Option<OpenPositionTrade> {
        self.open_positions.get(&symbol.id.sid).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.open_positions.is_empty()
    }

    pub fn record_fill(
        &mut self,
        symbol: &Symbol,
        event_time: DateTime,
        fill_price: Price,
        fill_quantity: Quantity,
        fees: Price,
    ) -> Option<Trade> {
        if fill_quantity.is_zero() {
            return None;
        }

        let sid = symbol.id.sid;
        let Some(open) = self.open_positions.remove(&sid) else {
            self.open_positions.insert(
                sid,
                OpenPositionTrade {
                    entry_time: event_time,
                    entry_price: fill_price,
                    quantity: fill_quantity,
                    fees,
                },
            );
            return None;
        };

        if open.quantity.is_zero() {
            self.open_positions.insert(
                sid,
                OpenPositionTrade {
                    entry_time: event_time,
                    entry_price: fill_price,
                    quantity: fill_quantity,
                    fees,
                },
            );
            return None;
        }

        if same_position_side(open.quantity, fill_quantity) {
            let new_quantity = open.quantity + fill_quantity;
            if !new_quantity.is_zero() {
                let new_price = ((open.entry_price * open.quantity.abs())
                    + (fill_price * fill_quantity.abs()))
                    / new_quantity.abs();
                self.open_positions.insert(
                    sid,
                    OpenPositionTrade {
                        entry_time: open.entry_time,
                        entry_price: new_price,
                        quantity: new_quantity,
                        fees: open.fees + fees,
                    },
                );
            }
            return None;
        }

        let close_abs = open.quantity.abs().min(fill_quantity.abs());
        let open_abs = open.quantity.abs();
        let fill_abs = fill_quantity.abs();
        let entry_fees = prorate(open.fees, close_abs, open_abs);
        let closing_fees = prorate(fees, close_abs, fill_abs);
        let remaining_fill_fees = fees - closing_fees;
        let remaining_open_fees = open.fees - entry_fees;
        let close_quantity = if open.quantity < Decimal::ZERO {
            -close_abs
        } else {
            close_abs
        };
        let trade = Trade::new(
            symbol.clone(),
            open.entry_time,
            event_time,
            open.entry_price,
            fill_price,
            close_quantity,
            entry_fees + closing_fees,
        );

        let remaining_quantity = open.quantity + fill_quantity;
        if !remaining_quantity.is_zero() {
            let remaining = if same_position_side(open.quantity, remaining_quantity) {
                OpenPositionTrade {
                    entry_time: open.entry_time,
                    entry_price: open.entry_price,
                    quantity: remaining_quantity,
                    fees: remaining_open_fees,
                }
            } else {
                OpenPositionTrade {
                    entry_time: event_time,
                    entry_price: fill_price,
                    quantity: remaining_quantity,
                    fees: remaining_fill_fees,
                }
            };
            self.open_positions.insert(sid, remaining);
        }

        Some(trade)
    }

    pub fn apply_split(&mut self, symbol: &Symbol, split_factor: Decimal) {
        if let Some(open) = self.open_positions.get_mut(&symbol.id.sid) {
            open.entry_price *= split_factor;
            if !split_factor.is_zero() {
                open.quantity /= split_factor;
            }
        }
    }
}

fn same_position_side(a: Decimal, b: Decimal) -> bool {
    (a > Decimal::ZERO && b > Decimal::ZERO) || (a < Decimal::ZERO && b < Decimal::ZERO)
}

fn prorate(value: Decimal, part: Decimal, total: Decimal) -> Decimal {
    if value.is_zero() || part.is_zero() || total.is_zero() {
        Decimal::ZERO
    } else {
        value * part / total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, Symbol};

    fn spy() -> Symbol {
        Symbol::create_equity("SPY", &Market::new(Market::USA))
    }

    #[test]
    fn long_trade_profit_uses_positive_quantity() {
        let trade = Trade::new(
            spy(),
            DateTime::EPOCH,
            DateTime::EPOCH,
            dec!(100),
            dec!(101),
            dec!(10),
            dec!(0),
        );

        assert_eq!(trade.pnl, dec!(10));
        assert_eq!(trade.pnl_pct, dec!(0.01));
        assert!(trade.is_win);
    }

    #[test]
    fn short_trade_loss_uses_negative_quantity() {
        let trade = Trade::new(
            spy(),
            DateTime::EPOCH,
            DateTime::EPOCH,
            dec!(100),
            dec!(101),
            dec!(-10),
            dec!(0),
        );

        assert_eq!(trade.pnl, dec!(-10));
        assert_eq!(trade.pnl_pct, dec!(-0.01));
        assert!(!trade.is_win);
    }

    #[test]
    fn short_trade_profit_uses_negative_quantity() {
        let trade = Trade::new(
            spy(),
            DateTime::EPOCH,
            DateTime::EPOCH,
            dec!(100),
            dec!(99),
            dec!(-10),
            dec!(0),
        );

        assert_eq!(trade.pnl, dec!(10));
        assert_eq!(trade.pnl_pct, dec!(0.01));
        assert!(trade.is_win);
    }

    #[test]
    fn trade_builder_keeps_residual_after_partial_close() {
        let sym = spy();
        let mut builder = TradeBuilder::new();

        assert!(builder
            .record_fill(&sym, DateTime::EPOCH, dec!(10), dec!(100), dec!(0))
            .is_none());
        let trade = builder
            .record_fill(&sym, DateTime::from_secs(2), dec!(12), dec!(-40), dec!(0))
            .unwrap();

        assert_eq!(trade.quantity, dec!(40));
        assert_eq!(trade.pnl, dec!(80));
        assert_eq!(builder.open_position(&sym).unwrap().quantity, dec!(60));

        let trade = builder
            .record_fill(&sym, DateTime::from_secs(3), dec!(11), dec!(-60), dec!(0))
            .unwrap();

        assert_eq!(trade.quantity, dec!(60));
        assert_eq!(trade.pnl, dec!(60));
        assert!(builder.is_empty());
    }

    #[test]
    fn trade_builder_handles_reversal() {
        let sym = spy();
        let mut builder = TradeBuilder::new();

        assert!(builder
            .record_fill(&sym, DateTime::from_secs(1), dec!(10), dec!(100), dec!(0))
            .is_none());
        let trade = builder
            .record_fill(&sym, DateTime::from_secs(2), dec!(12), dec!(-150), dec!(0))
            .unwrap();

        assert_eq!(trade.quantity, dec!(100));
        assert_eq!(trade.pnl, dec!(200));
        let open = builder.open_position(&sym).unwrap();
        assert_eq!(open.entry_price, dec!(12));
        assert_eq!(open.quantity, dec!(-50));

        let trade = builder
            .record_fill(&sym, DateTime::from_secs(3), dec!(10), dec!(20), dec!(0))
            .unwrap();

        assert_eq!(trade.quantity, dec!(-20));
        assert_eq!(trade.pnl, dec!(40));
        assert_eq!(builder.open_position(&sym).unwrap().quantity, dec!(-30));
    }

    #[test]
    fn trade_builder_prorates_entry_and_exit_fees_on_partial_close() {
        let sym = spy();
        let mut builder = TradeBuilder::new();

        assert!(builder
            .record_fill(&sym, DateTime::from_secs(1), dec!(10), dec!(100), dec!(10))
            .is_none());
        let trade = builder
            .record_fill(&sym, DateTime::from_secs(2), dec!(12), dec!(-40), dec!(4))
            .unwrap();

        assert_eq!(trade.fees, dec!(8));
        assert_eq!(trade.pnl, dec!(72));
        let open = builder.open_position(&sym).unwrap();
        assert_eq!(open.quantity, dec!(60));
        assert_eq!(open.fees, dec!(6));
    }

    #[test]
    fn trade_builder_splits_reversal_fill_fees_between_closed_and_new_position() {
        let sym = spy();
        let mut builder = TradeBuilder::new();

        assert!(builder
            .record_fill(&sym, DateTime::from_secs(1), dec!(10), dec!(100), dec!(10))
            .is_none());
        let trade = builder
            .record_fill(&sym, DateTime::from_secs(2), dec!(12), dec!(-150), dec!(15))
            .unwrap();

        assert_eq!(trade.fees, dec!(20));
        assert_eq!(trade.pnl, dec!(180));
        let open = builder.open_position(&sym).unwrap();
        assert_eq!(open.quantity, dec!(-50));
        assert_eq!(open.fees, dec!(5));
    }

    #[test]
    fn trade_builder_applies_split_to_open_position() {
        let sym = spy();
        let mut builder = TradeBuilder::new();
        builder.record_fill(&sym, DateTime::from_secs(1), dec!(100), dec!(10), dec!(0));

        builder.apply_split(&sym, dec!(0.5));

        let open = builder.open_position(&sym).unwrap();
        assert_eq!(open.entry_price, dec!(50.0));
        assert_eq!(open.quantity, dec!(20));
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TradeStatistics {
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub win_rate: Decimal,
    pub loss_rate: Decimal,
    pub average_win: Decimal,
    pub average_loss: Decimal,
    pub largest_win: Decimal,
    pub largest_loss: Decimal,
    pub profit_loss_ratio: Decimal,
    pub average_trade_duration_days: Decimal,
    pub max_consecutive_wins: usize,
    pub max_consecutive_losses: usize,
    pub expectancy: Decimal,
    pub total_net_profit: Decimal,
}

impl TradeStatistics {
    pub fn compute(trades: &[Trade]) -> Self {
        if trades.is_empty() {
            return Default::default();
        }

        let total = trades.len();
        let wins: Vec<&Trade> = trades.iter().filter(|t| t.is_win).collect();
        let losses: Vec<&Trade> = trades.iter().filter(|t| !t.is_win).collect();

        let win_count = wins.len();
        let loss_count = losses.len();

        let n = Decimal::from(total);
        let win_rate = if total == 0 {
            dec!(0)
        } else {
            Decimal::from(win_count) / n
        };
        let loss_rate = dec!(1) - win_rate;

        let avg_win = if win_count == 0 {
            dec!(0)
        } else {
            wins.iter().map(|t| t.pnl).sum::<Price>() / Decimal::from(win_count)
        };
        let avg_loss = if loss_count == 0 {
            dec!(0)
        } else {
            losses.iter().map(|t| t.pnl).sum::<Price>() / Decimal::from(loss_count)
        };

        let largest_win = wins.iter().map(|t| t.pnl).fold(dec!(0), |a, x| a.max(x));
        let largest_loss = losses.iter().map(|t| t.pnl).fold(dec!(0), |a, x| a.min(x));

        let profit_loss_ratio = if avg_loss.is_zero() {
            dec!(0)
        } else {
            (avg_win / avg_loss.abs()).abs()
        };

        let expectancy = (win_rate * avg_win) + (loss_rate * avg_loss);
        let total_net_profit = trades.iter().map(|t| t.pnl).sum();

        // Average trade duration in calendar days.
        let avg_duration_days = if total == 0 {
            dec!(0)
        } else {
            const NANOS_PER_DAY: f64 = 86_400.0 * 1_000_000_000.0;
            let total_days: f64 = trades
                .iter()
                .map(|t| (t.exit_time.0 - t.entry_time.0).abs() as f64 / NANOS_PER_DAY)
                .sum();
            Decimal::from_f64_retain(total_days / total as f64).unwrap_or(dec!(0))
        };

        // Max consecutive wins / losses.
        let (max_cons_wins, max_cons_losses) = {
            let mut max_w = 0usize;
            let mut max_l = 0usize;
            let mut cur_w = 0usize;
            let mut cur_l = 0usize;
            for t in trades {
                if t.is_win {
                    cur_w += 1;
                    cur_l = 0;
                    if cur_w > max_w {
                        max_w = cur_w;
                    }
                } else {
                    cur_l += 1;
                    cur_w = 0;
                    if cur_l > max_l {
                        max_l = cur_l;
                    }
                }
            }
            (max_w, max_l)
        };

        TradeStatistics {
            total_trades: total,
            winning_trades: win_count,
            losing_trades: loss_count,
            win_rate,
            loss_rate,
            average_win: avg_win,
            average_loss: avg_loss,
            largest_win,
            largest_loss,
            profit_loss_ratio,
            average_trade_duration_days: avg_duration_days,
            max_consecutive_wins: max_cons_wins,
            max_consecutive_losses: max_cons_losses,
            expectancy,
            total_net_profit,
        }
    }
}
