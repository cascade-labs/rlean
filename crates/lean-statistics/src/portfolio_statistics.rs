use crate::statistics::Statistics;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioStatistics {
    pub average_win_rate: Decimal,
    pub average_loss_rate: Decimal,
    pub profit_loss_ratio: Decimal,
    pub win_rate: Decimal,
    pub loss_rate: Decimal,
    pub expectancy: Decimal,
    pub compounding_annual_return: Decimal,
    pub drawdown: Decimal,
    pub total_net_profit: Decimal,
    pub sharpe_ratio: Decimal,
    pub sortino_ratio: Decimal,
    pub probabilistic_sharpe_ratio: Decimal,
    pub alpha: Decimal,
    pub beta: Decimal,
    pub annual_standard_deviation: Decimal,
    pub annual_variance: Decimal,
    pub information_ratio: Decimal,
    pub tracking_error: Decimal,
    pub treynor_ratio: Decimal,
    pub portfolio_turnover: Decimal,
    pub omega_ratio: Decimal,
    pub recovery_factor: Decimal,
    pub calmar_ratio: Decimal,
    pub max_consecutive_wins: usize,
    pub max_consecutive_losses: usize,
    pub average_trade_duration_days: Decimal,
    pub total_trades: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub largest_win: Decimal,
    pub largest_loss: Decimal,
}

impl PortfolioStatistics {
    pub fn compute(
        equity_curve: &[Decimal],
        benchmark_curve: &[Decimal],
        trades: &[crate::trade_statistics::Trade],
        trading_days: i64,
        starting_cash: Decimal,
        risk_free_rate: Decimal,
    ) -> Self {
        let trade_stats = crate::trade_statistics::TradeStatistics::compute(trades);

        let total_return = if starting_cash.is_zero() { dec!(0) } else {
            let final_equity = equity_curve.last().copied().unwrap_or(starting_cash);
            (final_equity - starting_cash) / starting_cash
        };

        let daily_returns: Vec<Decimal> = equity_curve.windows(2)
            .map(|w| if w[0].is_zero() { dec!(0) } else { (w[1] - w[0]) / w[0] })
            .collect();

        let benchmark_returns: Vec<Decimal> = benchmark_curve.windows(2)
            .map(|w| if w[0].is_zero() { dec!(0) } else { (w[1] - w[0]) / w[0] })
            .collect();

        let annual_return = Statistics::annual_performance(total_return, trading_days);
        let drawdown = Statistics::max_drawdown(equity_curve);
        let daily_rf = risk_free_rate / dec!(252);
        let sharpe = Statistics::sharpe_ratio(&daily_returns, daily_rf);
        let sortino = Statistics::sortino_ratio(&daily_returns, daily_rf);
        let beta = Statistics::beta(&daily_returns, &benchmark_returns);
        let bench_annual = if benchmark_returns.len() >= 2 {
            Statistics::annual_performance(
                benchmark_returns.iter().product::<Decimal>(),
                trading_days,
            )
        } else { dec!(0) };
        let alpha = Statistics::alpha(annual_return, beta, bench_annual, risk_free_rate);
        let tracking_error = Statistics::tracking_error(&daily_returns, &benchmark_returns);
        let information_ratio = Statistics::information_ratio(&daily_returns, &benchmark_returns);
        let psr = Statistics::probabilistic_sharpe_ratio(&daily_returns, daily_rf, 0.0);
        let omega = Statistics::omega_ratio(&daily_returns, dec!(0));

        use rust_decimal::prelude::ToPrimitive;
        let n = Decimal::from(daily_returns.len());
        let mean_r = if n.is_zero() { dec!(0) } else { daily_returns.iter().sum::<Decimal>() / n };
        let variance = if n <= dec!(1) { dec!(0) } else {
            daily_returns.iter().map(|r| (r - mean_r) * (r - mean_r)).sum::<Decimal>() / (n - dec!(1))
        };
        let ann_std = variance.to_f64().unwrap_or(0.0).sqrt() * 252_f64.sqrt();
        let annual_std = Decimal::from_f64_retain(ann_std).unwrap_or(dec!(0));

        // Max drawdown in dollar terms for recovery factor.
        let max_dd_dollars = {
            let mut peak = equity_curve.first().copied().unwrap_or(starting_cash);
            let mut max_loss = dec!(0);
            for &eq in equity_curve {
                if eq > peak { peak = eq; }
                let loss = peak - eq;
                if loss > max_loss { max_loss = loss; }
            }
            max_loss
        };
        let recovery = Statistics::recovery_factor(trade_stats.total_net_profit, max_dd_dollars);
        let calmar = Statistics::calmar_ratio(annual_return, drawdown);

        // Portfolio turnover: total trading value (both legs) / (average equity * 2).
        // Clamped to [0, 1].
        let portfolio_turnover = {
            let total_trading_value: Decimal = trades.iter().map(|t| {
                let qty_abs = t.quantity.abs();
                (t.entry_price * qty_abs) + (t.exit_price * qty_abs)
            }).sum();

            let n_eq = Decimal::from(equity_curve.len());
            let average_equity = if n_eq.is_zero() {
                dec!(0)
            } else {
                equity_curve.iter().sum::<Decimal>() / n_eq
            };

            let denominator = average_equity * dec!(2);
            if denominator.is_zero() {
                dec!(0)
            } else {
                let raw = total_trading_value / denominator;
                raw.max(dec!(0)).min(dec!(1))
            }
        };

        PortfolioStatistics {
            average_win_rate: trade_stats.average_win,
            average_loss_rate: trade_stats.average_loss,
            profit_loss_ratio: trade_stats.profit_loss_ratio,
            win_rate: trade_stats.win_rate,
            loss_rate: trade_stats.loss_rate,
            expectancy: trade_stats.expectancy,
            compounding_annual_return: annual_return,
            drawdown,
            total_net_profit: trade_stats.total_net_profit,
            sharpe_ratio: sharpe,
            sortino_ratio: sortino,
            probabilistic_sharpe_ratio: psr,
            alpha,
            beta,
            annual_standard_deviation: annual_std,
            annual_variance: annual_std * annual_std,
            information_ratio,
            tracking_error,
            treynor_ratio: if beta.is_zero() { dec!(0) } else { (annual_return - risk_free_rate) / beta },
            portfolio_turnover,
            omega_ratio: omega,
            recovery_factor: recovery,
            calmar_ratio: calmar,
            max_consecutive_wins: trade_stats.max_consecutive_wins,
            max_consecutive_losses: trade_stats.max_consecutive_losses,
            average_trade_duration_days: trade_stats.average_trade_duration_days,
            total_trades: trade_stats.total_trades,
            winning_trades: trade_stats.winning_trades,
            losing_trades: trade_stats.losing_trades,
            largest_win: trade_stats.largest_win,
            largest_loss: trade_stats.largest_loss,
        }
    }
}
