use crate::{
    algorithm_manager::AlgorithmManager, data_manager::DataManager, engine_config::EngineConfig,
    result_handler::ResultHandler,
};
use lean_algorithm::algorithm::IAlgorithm;
use lean_core::Result as LeanResult;
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

        algorithm.initialize()?;

        let start = algorithm.start_date();
        let end = algorithm.end_date();
        let starting_cash = algorithm.starting_cash();
        let subscriptions = algorithm.subscriptions();

        let mut data_manager = DataManager::new(self.config.data_root.clone());
        data_manager
            .initialize_feed(&subscriptions, start, end)
            .await?;

        let transaction_manager = Arc::new(TransactionManager::new());

        let fill_model = ImmediateFillModel::new(Box::new(NullSlippageModel));
        let order_processor =
            OrderProcessor::new(Box::new(fill_model), transaction_manager.clone());

        let mut result_handler = ResultHandler::new();
        let mut algo_manager = AlgorithmManager::new(algorithm);

        let mut trading_days = 0i64;
        let mut last_date = None::<chrono::NaiveDate>;
        let mut slices_processed = 0i64;

        while let Some(slice) = data_manager.next_slice().await? {
            if slice.time > end {
                break;
            }
            if !slice.has_data {
                continue;
            }

            let slice_date = slice.time.date_utc();
            if last_date.map(|prev| prev != slice_date).unwrap_or(true) {
                if last_date.is_some() {
                    algo_manager.on_end_of_day(None);
                }
                trading_days += 1;
            }
            last_date = Some(slice_date);
            slices_processed += 1;

            let bars_map: std::collections::HashMap<u64, lean_data::TradeBar> =
                slice.bars.iter().map(|(k, v)| (*k, v.clone())).collect();

            let order_events = order_processor.process_orders(&bars_map, slice.time);

            for event in &order_events {
                algo_manager.on_order_event(event);
            }

            algo_manager.on_data(&slice);

            let portfolio_value = algo_manager.algorithm.portfolio_value();
            result_handler.record_equity(slice.time, portfolio_value);
        }

        if slices_processed > 0 {
            algo_manager.on_end_of_day(None);
        }

        algo_manager.on_end_of_algorithm();

        result_handler.finalize(&[], trading_days, starting_cash);
        result_handler.print_summary();

        info!(
            "Backtest complete. {} trading days, {} slices processed.",
            trading_days, slices_processed
        );
        Ok(result_handler)
    }
}
