use crate::{
    algorithm_manager::AlgorithmManager, data_manager::DataManager, engine_config::EngineConfig,
    result_handler::ResultHandler,
};
use lean_algorithm::algorithm::IAlgorithm;
use lean_core::{DateTime, Result as LeanResult};
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    transaction_manager::TransactionManager,
};
use std::sync::Arc;
use tracing::info;

/// The main backtesting engine. Drives the time loop.
pub struct BacktestEngine {
    config: EngineConfig,
}

impl BacktestEngine {
    pub fn new(config: EngineConfig) -> Self {
        BacktestEngine { config }
    }

    pub async fn run(&self, mut algorithm: Box<dyn IAlgorithm>) -> LeanResult<ResultHandler> {
        info!("Starting backtest: {}", algorithm.name());

        // Initialize algorithm
        algorithm.initialize()?;

        let start = algorithm.start_date();
        let end = algorithm.end_date();
        // Read starting cash from the algorithm after initialize() has run (e.g. set_cash was called).
        // Falls back to the trait default (100,000) if the implementor does not override starting_cash().
        let starting_cash = algorithm.starting_cash();

        let data_manager = DataManager::new(self.config.data_root.clone());
        let transaction_manager = Arc::new(TransactionManager::new());

        let fill_model = ImmediateFillModel::new(Box::new(NullSlippageModel));
        let order_processor =
            OrderProcessor::new(Box::new(fill_model), transaction_manager.clone());

        let mut result_handler = ResultHandler::new();
        let mut algo_manager = AlgorithmManager::new(algorithm);

        // Date loop
        let start_date = start.date_utc();
        let end_date = end.date_utc();
        let mut current_date = start_date;
        let mut trading_days = 0i64;

        while current_date <= end_date {
            let slice = data_manager.get_slice_for_date(current_date).await?;

            if !slice.has_data {
                current_date += chrono::Duration::days(1);
                continue;
            }

            trading_days += 1;

            // Update portfolio prices and process orders
            let bars_map: std::collections::HashMap<u64, lean_data::TradeBar> =
                slice.bars.iter().map(|(k, v)| (*k, v.clone())).collect();

            use chrono::{TimeZone, Utc};
            let utc_time =
                DateTime::from(Utc.from_utc_datetime(&current_date.and_hms_opt(16, 0, 0).unwrap()));

            let order_events = order_processor.process_orders(&bars_map, utc_time);

            // Notify algorithm of fills
            for event in &order_events {
                algo_manager.on_order_event(event);
            }

            // Deliver data to algorithm
            algo_manager.on_data(&slice);

            // End of day
            algo_manager.on_end_of_day(None);

            // Compute real portfolio value (cash + market value of all holdings).
            let portfolio_value = algo_manager.algorithm.portfolio_value();
            result_handler.record_equity(utc_time, portfolio_value);

            current_date += chrono::Duration::days(1);
        }

        algo_manager.on_end_of_algorithm();

        result_handler.finalize(&[], trading_days, starting_cash);
        result_handler.print_summary();

        info!(
            "Backtest complete. {} trading days processed.",
            trading_days
        );
        Ok(result_handler)
    }
}
