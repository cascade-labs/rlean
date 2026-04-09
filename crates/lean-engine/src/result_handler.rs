use lean_core::{DateTime, Price};
use lean_statistics::{PortfolioStatistics, TradeStatistics};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Accumulates equity curve and final results during a backtest.
#[derive(Debug, Default)]
pub struct ResultHandler {
    pub equity_curve: BTreeMap<i64, Price>, // time_ns -> equity
    pub benchmark_curve: BTreeMap<i64, Price>,
    pub portfolio_stats: Option<PortfolioStatistics>,
}

impl ResultHandler {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn record_equity(&mut self, time: DateTime, equity: Price) {
        self.equity_curve.insert(time.0, equity);
    }

    pub fn record_benchmark(&mut self, time: DateTime, price: Price) {
        self.benchmark_curve.insert(time.0, price);
    }

    pub fn finalize(
        &mut self,
        trades: &[lean_statistics::Trade],
        trading_days: i64,
        starting_cash: Price,
    ) {
        let equity_vec: Vec<Price> = self.equity_curve.values().cloned().collect();
        let bench_vec: Vec<Price> = self.benchmark_curve.values().cloned().collect();

        use rust_decimal_macros::dec;
        self.portfolio_stats = Some(PortfolioStatistics::compute(
            &equity_vec,
            &bench_vec,
            trades,
            trading_days,
            starting_cash,
            dec!(0.04) / dec!(252), // 4% annual risk-free rate, daily
        ));
    }

    pub fn print_summary(&self) {
        if let Some(stats) = &self.portfolio_stats {
            println!("═══════════════════════════════════════════════");
            println!("  BACKTEST RESULTS");
            println!("═══════════════════════════════════════════════");
            println!("  Annual Return:     {:.2}%", stats.compounding_annual_return * rust_decimal_macros::dec!(100));
            println!("  Max Drawdown:      {:.2}%", stats.drawdown * rust_decimal_macros::dec!(100));
            println!("  Sharpe Ratio:      {:.3}", stats.sharpe_ratio);
            println!("  Sortino Ratio:     {:.3}", stats.sortino_ratio);
            println!("  Win Rate:          {:.1}%", stats.win_rate * rust_decimal_macros::dec!(100));
            println!("  Profit/Loss:       {:.2}", stats.profit_loss_ratio);
            println!("  Alpha:             {:.4}", stats.alpha);
            println!("  Beta:              {:.4}", stats.beta);
            println!("  Net Profit:        ${:.2}", stats.total_net_profit);
            println!("═══════════════════════════════════════════════");
        }
    }
}
