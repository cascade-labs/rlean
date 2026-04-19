/// SMA Crossover Strategy — the "Hello World" of quantitative trading.
/// Buy when fast SMA crosses above slow SMA; sell when it crosses below.
///
/// This example shows how to:
/// - Implement the IAlgorithm trait
/// - Subscribe to equity data
/// - Create and update indicators
/// - Place market orders based on signals
/// - Run through the BacktestEngine
use lean_algorithm::{
    algorithm::{AlgorithmStatus, IAlgorithm},
    qc_algorithm::QcAlgorithm,
};
use lean_core::{DateTime, Resolution, Symbol};
use lean_data::Slice;
use lean_engine::{BacktestEngine, EngineConfig};
use lean_indicators::{indicator::Indicator, Sma};
use lean_orders::OrderEvent;
use rust_decimal_macros::dec;

struct SmaCrossover {
    algo: QcAlgorithm,
    spy: Option<Symbol>,
    fast: Sma,
    slow: Sma,
    invested: bool,
}

impl SmaCrossover {
    fn new() -> Self {
        SmaCrossover {
            algo: QcAlgorithm::new("SMA Crossover", dec!(100_000)),
            spy: None,
            fast: Sma::new(50),
            slow: Sma::new(200),
            invested: false,
        }
    }
}

impl IAlgorithm for SmaCrossover {
    fn initialize(&mut self) -> lean_core::Result<()> {
        self.algo.set_start_date(2020, 1, 1);
        self.algo.set_end_date(2024, 1, 1);
        self.algo.set_cash(dec!(100_000));

        let spy = self.algo.add_equity("SPY", Resolution::Daily);
        self.spy = Some(spy);

        self.algo.log_message("SMA Crossover initialized.");
        Ok(())
    }

    fn on_data(&mut self, slice: &Slice) {
        let spy = match &self.spy {
            Some(s) => s.clone(),
            None => return,
        };

        let bar = match slice.get_bar(&spy) {
            Some(b) => b,
            None => return,
        };

        let fast = self.fast.update_bar(bar);
        let slow = self.slow.update_bar(bar);

        if !fast.is_ready() || !slow.is_ready() {
            return;
        }

        if fast.value > slow.value && !self.invested {
            // Golden cross — go long
            self.algo.set_holdings(&spy, dec!(1));
            self.invested = true;
            self.algo.log_message(format!(
                "BUY SPY @ {} | Fast SMA: {:.2} > Slow SMA: {:.2}",
                bar.close, fast.value, slow.value
            ));
        } else if fast.value < slow.value && self.invested {
            // Death cross — liquidate
            self.algo.liquidate(Some(&spy));
            self.invested = false;
            self.algo.log_message(format!(
                "SELL SPY @ {} | Fast SMA: {:.2} < Slow SMA: {:.2}",
                bar.close, fast.value, slow.value
            ));
        }
    }

    fn on_order_event(&mut self, event: &OrderEvent) {
        if event.is_fill() {
            self.algo.log_message(format!(
                "Order {} filled: {} @ {}",
                event.order_id, event.fill_quantity, event.fill_price
            ));
        }
    }

    fn on_end_of_algorithm(&mut self) {
        println!("Final portfolio value: ${:.2}", self.algo.portfolio_value());
        println!("Cash: ${:.2}", self.algo.cash());
    }

    fn name(&self) -> &str {
        "SmaCrossover"
    }
    fn start_date(&self) -> DateTime {
        self.algo.start_date
    }
    fn end_date(&self) -> DateTime {
        self.algo.end_date
    }
    fn status(&self) -> AlgorithmStatus {
        self.algo.status
    }
    fn portfolio_value(&self) -> lean_core::Price {
        self.algo.portfolio_value()
    }
    fn starting_cash(&self) -> lean_core::Price {
        self.algo.cash()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let config = EngineConfig {
        data_root: std::path::PathBuf::from("data"),
        ..Default::default()
    };

    let engine = BacktestEngine::new(config);
    let algo = Box::new(SmaCrossover::new());

    match engine.run(algo).await {
        Ok(results) => {
            println!("\nBacktest complete.");
            results.print_summary();
        }
        Err(e) => eprintln!("Backtest failed: {}", e),
    }
}
