use rlean_core::{DateTime, NanosecondTimestamp, Price};
use rlean_statistics::PortfolioStatistics;
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
        trades: &[rlean_statistics::Trade],
        trading_days: i64,
        starting_cash: Price,
    ) {
        let equity_vec = daily_close_values(&self.equity_curve);
        let bench_vec = daily_close_values(&self.benchmark_curve);

        use rust_decimal_macros::dec;
        self.portfolio_stats = Some(PortfolioStatistics::compute(
            &equity_vec,
            &bench_vec,
            trades,
            trading_days,
            starting_cash,
            dec!(0.04),
        ));
    }

    pub fn print_summary(&self) {
        if let Some(stats) = &self.portfolio_stats {
            println!("═══════════════════════════════════════════════");
            println!("  BACKTEST RESULTS");
            println!("═══════════════════════════════════════════════");
            println!(
                "  Annual Return:     {:.2}%",
                stats.compounding_annual_return * rust_decimal_macros::dec!(100)
            );
            println!(
                "  Max Drawdown:      {:.2}%",
                stats.drawdown * rust_decimal_macros::dec!(100)
            );
            println!("  Sharpe Ratio:      {:.3}", stats.sharpe_ratio);
            println!("  Sortino Ratio:     {:.3}", stats.sortino_ratio);
            println!(
                "  Win Rate:          {:.1}%",
                stats.win_rate * rust_decimal_macros::dec!(100)
            );
            println!("  Profit/Loss:       {:.2}", stats.profit_loss_ratio);
            println!("  Alpha:             {:.4}", stats.alpha);
            println!("  Beta:              {:.4}", stats.beta);
            println!("  Net Profit:        ${:.2}", stats.total_net_profit);
            println!("═══════════════════════════════════════════════");
        }
    }
}

fn daily_close_values(curve: &BTreeMap<i64, Price>) -> Vec<Price> {
    let mut last_by_date = BTreeMap::new();

    for (&time, &value) in curve {
        let date = NanosecondTimestamp(time).date_utc();
        last_by_date.insert(date, value);
    }

    last_by_date.values().copied().collect()
}

#[cfg(test)]
mod tests {
    use super::{daily_close_values, ResultHandler};
    use chrono::{TimeZone, Utc};
    use rlean_core::{DateTime, NanosecondTimestamp};
    use rust_decimal_macros::dec;
    use std::collections::BTreeMap;

    fn timestamp(year: i32, month: u32, day: u32, hour: u32) -> DateTime {
        NanosecondTimestamp::from(
            Utc.with_ymd_and_hms(year, month, day, hour, 0, 0)
                .single()
                .unwrap(),
        )
    }

    #[test]
    fn daily_close_values_keep_last_sample_per_date() {
        let mut curve = BTreeMap::new();
        curve.insert(timestamp(2024, 1, 2, 14).0, dec!(100));
        curve.insert(timestamp(2024, 1, 2, 21).0, dec!(101));
        curve.insert(timestamp(2024, 1, 3, 14).0, dec!(102));
        curve.insert(timestamp(2024, 1, 3, 21).0, dec!(103));

        assert_eq!(daily_close_values(&curve), vec![dec!(101), dec!(103)]);
    }

    #[test]
    fn finalize_computes_daily_statistics_from_daily_close_equity() {
        let mut result_handler = ResultHandler::new();
        result_handler.record_equity(timestamp(2024, 1, 2, 14), dec!(100000));
        result_handler.record_equity(timestamp(2024, 1, 2, 21), dec!(101000));
        result_handler.record_equity(timestamp(2024, 1, 3, 14), dec!(101500));
        result_handler.record_equity(timestamp(2024, 1, 3, 21), dec!(102000));
        result_handler.record_equity(timestamp(2024, 1, 4, 14), dec!(102500));
        result_handler.record_equity(timestamp(2024, 1, 4, 21), dec!(103000));
        result_handler.record_equity(timestamp(2024, 1, 5, 14), dec!(103500));
        result_handler.record_equity(timestamp(2024, 1, 5, 21), dec!(104000));

        result_handler.finalize(&[], 4, dec!(100000));

        let stats = result_handler.portfolio_stats.unwrap();
        let expected = rlean_statistics::PortfolioStatistics::compute(
            &[dec!(101000), dec!(102000), dec!(103000), dec!(104000)],
            &[],
            &[],
            4,
            dec!(100000),
            dec!(0.04),
        );

        assert_eq!(stats.sharpe_ratio, expected.sharpe_ratio);
        assert_eq!(
            stats.annual_standard_deviation,
            expected.annual_standard_deviation
        );
    }
}
