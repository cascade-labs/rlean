use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

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
