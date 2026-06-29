use crate::{
    algorithm_manager::AlgorithmManager,
    runner::backtest::{
        benchmark_subscription_for_symbol, subscriptions_with_benchmark,
        subscriptions_with_option_chains,
    },
    LiveRunConfig, LiveRunResult,
};
use anyhow::Result;
use crossbeam_channel::RecvTimeoutError;
use lean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices};
use lean_core::MarketHoursDatabase;
use lean_data::{LiveDataItem, LiveDataSubscription, SubscriptionDataConfig};
use lean_live::LiveSliceAssembler;
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
    OrderEvent,
};
use lean_statistics::{Trade, TradeBuilder};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Engine-owned live runner entry point.
///
/// All strategy languages enter through `lean_algorithm::lifecycle::AlgorithmBridge`; language
/// crates do not provide runner futures or alternate loops.
pub async fn run_live<B>(bridge: B, config: LiveRunConfig) -> Result<LiveRunResult>
where
    B: AlgorithmBridge,
{
    let runtime_context = crate::AlgorithmRuntimeContext::new(
        config.data_root.clone(),
        config.data_store.clone(),
        config.history_provider.clone(),
        config.custom_data_sources.clone(),
        config.parameters.clone(),
    );
    run_live_with_runtime(bridge, config, runtime_context).await
}

pub async fn run_live_with_runtime<B>(
    bridge: B,
    mut config: LiveRunConfig,
    runtime_context: crate::AlgorithmRuntimeContext,
) -> Result<LiveRunResult>
where
    B: AlgorithmBridge,
{
    let started_at = chrono::Utc::now();
    let mut services =
        crate::EngineAlgorithmServices::new(lean_core::DateTime::now(), runtime_context.clone());
    let mut algorithm_manager = AlgorithmManager::new(bridge, runtime_context);
    let market_hours_database = MarketHoursDatabase::global();
    algorithm_manager.set_market_hours_database(market_hours_database);

    let brokerage_sync =
        crate::live::brokerage::sync_brokerage_state(config.brokerage.as_mut()).await?;
    let mut snapshot = crate::live::snapshots::LiveDeploymentSnapshot::new(
        config
            .output_dir
            .as_ref()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("live")
            .to_string(),
    );
    snapshot.recent_order_events = brokerage_sync.order_events.clone();

    algorithm_manager.initialize(&mut services)?;
    let benchmark_subscription = benchmark_subscription_for_symbol(
        &algorithm_manager.benchmark_symbol(),
        algorithm_manager.subscriptions(),
    );
    let subscriptions =
        subscriptions_with_benchmark(algorithm_manager.subscriptions(), benchmark_subscription);
    let subscriptions =
        subscriptions_with_option_chains(subscriptions, &algorithm_manager.option_subscriptions());
    algorithm_manager.prepare_data_delivery(&subscriptions)?;
    algorithm_manager.warmup_finished(&mut services);

    let mut live_subscriptions = LiveSubscriptionSet::subscribe_initial(
        &mut config.live_data_queue,
        &subscriptions,
        config
            .brokerage_model
            .as_ref()
            .map(|model| format!("{model:?}")),
        config.paper_trading,
        &config.parameters,
    )?;

    let transactions = algorithm_manager.transactions();
    let portfolio = algorithm_manager.portfolio();
    let order_processor = transactions.as_ref().map(|tm| {
        OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            tm.clone(),
        )
    });
    let mut all_order_events: Vec<OrderEvent> = brokerage_sync.order_events;
    let mut trade_builder = TradeBuilder::new();
    let mut completed_trades = Vec::new();
    let run_started = Instant::now();
    let mut assembler = LiveSliceAssembler::new();

    'live: loop {
        if should_stop(
            algorithm_manager.slices_processed() as usize,
            config.max_slices,
            run_started,
            config.max_runtime,
        ) {
            break;
        }

        let Some(item) = next_live_item(&mut live_subscriptions, Duration::from_millis(250))?
        else {
            continue;
        };

        for slice in assembler.push(item) {
            process_live_slice(
                &mut algorithm_manager,
                &mut services,
                &mut config,
                &mut live_subscriptions,
                &order_processor,
                portfolio.as_ref(),
                &mut all_order_events,
                &mut trade_builder,
                &mut completed_trades,
                &slice,
            )
            .await?;

            if algorithm_manager.algorithm().terminal_status().is_some()
                || algorithm_manager.algorithm().runtime_error().is_some()
            {
                break 'live;
            }
            if should_stop(
                algorithm_manager.slices_processed() as usize,
                config.max_slices,
                run_started,
                config.max_runtime,
            ) {
                break;
            }
        }
    }

    if algorithm_manager.algorithm().terminal_status().is_none()
        && algorithm_manager.algorithm().runtime_error().is_none()
    {
        if let Some(slice) = assembler.flush() {
            if !should_stop(
                algorithm_manager.slices_processed() as usize,
                config.max_slices,
                run_started,
                config.max_runtime,
            ) {
                process_live_slice(
                    &mut algorithm_manager,
                    &mut services,
                    &mut config,
                    &mut live_subscriptions,
                    &order_processor,
                    portfolio.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                    &slice,
                )
                .await?;
            }
        }
    }

    algorithm_manager.finish(&mut services);
    live_subscriptions.unsubscribe_all(&mut config.live_data_queue);
    snapshot.slices_processed = algorithm_manager.slices_processed() as usize;
    snapshot.final_value = algorithm_manager
        .portfolio_value()
        .to_string()
        .parse::<f64>()
        .unwrap_or(0.0);
    snapshot.recent_order_events = all_order_events.clone();

    Ok(LiveRunResult {
        slices_processed: algorithm_manager.slices_processed() as usize,
        final_value: algorithm_manager
            .portfolio_value()
            .to_string()
            .parse::<f64>()
            .unwrap_or(0.0),
        order_events: all_order_events,
        started_at,
        stopped_at: chrono::Utc::now(),
    })
}

#[allow(clippy::too_many_arguments)]
async fn process_live_slice<B: AlgorithmBridge>(
    algorithm_manager: &mut AlgorithmManager<B>,
    services: &mut dyn AlgorithmServices,
    config: &mut LiveRunConfig,
    live_subscriptions: &mut LiveSubscriptionSet,
    order_processor: &Option<OrderProcessor>,
    portfolio: Option<&std::sync::Arc<lean_algorithm::portfolio::SecurityPortfolioManager>>,
    all_order_events: &mut Vec<OrderEvent>,
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
    slice: &lean_data::Slice,
) -> Result<()> {
    if !slice.has_data {
        return Ok(());
    }
    let slice_arc = Arc::new(slice.clone());

    let new_trading_day = algorithm_manager.handle_new_trading_day(slice, services);
    let changes = algorithm_manager.apply_universe_selection(slice, new_trading_day, services);
    if changes.has_changes() {
        let subscriptions = subscriptions_with_option_chains(
            algorithm_manager.subscriptions(),
            &algorithm_manager.option_subscriptions(),
        );
        live_subscriptions.sync(config, &subscriptions)?;
    }

    algorithm_manager.advance_frontier(slice, services);
    let option_chains: Vec<(&str, &lean_data::OptionChain)> = slice
        .option_chains
        .iter()
        .map(|(key, chain)| (key.as_str(), chain.as_ref()))
        .collect();
    algorithm_manager.process_order_events(
        slice,
        &option_chains,
        order_processor.as_ref(),
        portfolio,
        services,
        all_order_events,
        trade_builder,
        completed_trades,
    );

    algorithm_manager.deliver_data(
        lean_algorithm::algorithm::DataDeliveryPayload {
            slice: slice_arc,
        },
        services,
    );
    algorithm_manager.run_framework(slice, services);
    algorithm_manager.end_time_step(services);
    Ok(())
}

fn should_stop(
    slices_processed: usize,
    max_slices: Option<usize>,
    started: Instant,
    max_runtime: Option<Duration>,
) -> bool {
    max_slices
        .map(|max_slices| slices_processed >= max_slices)
        .unwrap_or(false)
        || max_runtime
            .map(|max_runtime| started.elapsed() >= max_runtime)
            .unwrap_or(false)
}

fn next_live_item(
    subscriptions: &mut LiveSubscriptionSet,
    timeout: Duration,
) -> Result<Option<LiveDataItem>> {
    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(Duration::ZERO) {
            Ok(item) => return Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }
    }

    if subscriptions.market.is_empty() {
        return Ok(None);
    }

    for subscription in subscriptions.market.values() {
        match subscription.receiver.recv_timeout(timeout) {
            Ok(item) => return Ok(Some(item?)),
            Err(RecvTimeoutError::Timeout) => return Ok(None),
            Err(RecvTimeoutError::Disconnected) => {}
        }
    }
    Ok(None)
}

struct LiveSubscriptionSet {
    market: HashMap<u64, LiveDataSubscription>,
    configs: HashMap<u64, SubscriptionDataConfig>,
}

impl LiveSubscriptionSet {
    fn subscribe_initial(
        queue: &mut lean_live::DataQueueHandlerManager,
        subscriptions: &[SubscriptionDataConfig],
        brokerage: Option<String>,
        paper_trading: bool,
        parameters: &HashMap<String, String>,
    ) -> Result<Self> {
        let job = lean_data::LiveNodePacket {
            brokerage: brokerage.unwrap_or_else(|| "Default".to_string()),
            data_queue_handlers: Vec::new(),
            brokerage_data: HashMap::new(),
            parameters: parameters.clone(),
            paper_trading,
        };
        queue.set_job(&job)?;

        let mut set = Self {
            market: HashMap::new(),
            configs: HashMap::new(),
        };
        for config in subscriptions {
            set.add(queue, config.clone())?;
        }
        Ok(set)
    }

    fn sync(
        &mut self,
        config: &mut LiveRunConfig,
        current: &[SubscriptionDataConfig],
    ) -> Result<()> {
        let desired: HashSet<u64> = current
            .iter()
            .map(SubscriptionDataConfig::unique_id)
            .collect();
        let existing: Vec<u64> = self.configs.keys().copied().collect();
        for id in existing {
            if !desired.contains(&id) {
                if let Some(subscription_config) = self.configs.remove(&id) {
                    config.live_data_queue.unsubscribe(&subscription_config)?;
                }
                self.market.remove(&id);
            }
        }

        for subscription_config in current {
            if !self.configs.contains_key(&subscription_config.unique_id()) {
                self.add(&mut config.live_data_queue, subscription_config.clone())?;
            }
        }
        Ok(())
    }

    fn add(
        &mut self,
        queue: &mut lean_live::DataQueueHandlerManager,
        config: SubscriptionDataConfig,
    ) -> Result<()> {
        let id = config.unique_id();
        let subscription = queue.subscribe(&config)?;
        self.configs.insert(id, config);
        self.market.insert(id, subscription);
        Ok(())
    }

    fn unsubscribe_all(&mut self, queue: &mut lean_live::DataQueueHandlerManager) {
        let configs: Vec<_> = self.configs.drain().map(|(_, config)| config).collect();
        for config in configs {
            if let Err(error) = queue.unsubscribe(&config) {
                tracing::warn!(
                    "failed to unsubscribe live data for {}: {error}",
                    config.symbol
                );
            }
        }
        self.market.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_algorithm::{
        algorithm::{AlgorithmStatus, DataDeliveryPayload, SecurityChanges},
        lifecycle::{AlgorithmServices, OptionSubscription, UniverseSelection},
    };
    use lean_core::{
        DataNormalizationMode, DateTime, LeanError, Market, Resolution, Result, Symbol, TimeSpan,
    };
    use lean_data::{
        live_data_channel, DataQueueHandler, LiveDataItem, LiveDataSubscriptionConfig, TradeBar,
        TradeBarData,
    };
    use lean_orders::{Order, OrderEvent, TransactionManager};
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct EventLog(Arc<Mutex<Vec<String>>>);

    impl EventLog {
        fn push(&self, event: impl Into<String>) {
            self.0.lock().unwrap().push(event.into());
        }

        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn position(&self, event: &str) -> usize {
            self.entries()
                .iter()
                .position(|entry| entry == event)
                .unwrap_or_else(|| panic!("event {event} not found in {:?}", self.entries()))
        }

        fn count(&self, event: &str) -> usize {
            self.entries()
                .iter()
                .filter(|entry| entry.as_str() == event)
                .count()
        }
    }

    struct RecordingLiveHandler {
        name: &'static str,
        accepts: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }

    struct ScriptedLiveHandler {
        log: EventLog,
        items: Vec<LiveDataItem>,
    }

    impl DataQueueHandler for ScriptedLiveHandler {
        fn set_job(&mut self, _job: &lean_data::LiveNodePacket) -> Result<()> {
            self.log.push("queue:set_job");
            Ok(())
        }

        fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
            self.log
                .push(format!("queue:subscribe:{}", config.symbol.value));
            let (sender, receiver) = live_data_channel();
            for item in self.items.clone() {
                sender.send(Ok(item)).unwrap();
            }
            Ok(LiveDataSubscription::new(
                LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
                receiver,
            ))
        }

        fn unsubscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()> {
            self.log
                .push(format!("queue:unsubscribe:{}", config.symbol.value));
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            "scripted"
        }
    }

    #[derive(Clone)]
    struct RecordingBridge {
        log: EventLog,
        subscriptions: Arc<Mutex<Vec<SubscriptionDataConfig>>>,
        universe_changes: Arc<Mutex<Vec<UniverseSelection>>>,
        terminal_status_after_data: Arc<Mutex<Option<AlgorithmStatus>>>,
        runtime_error_after_data: Arc<Mutex<Option<String>>>,
        terminal_status: Arc<Mutex<Option<AlgorithmStatus>>>,
        runtime_error: Arc<Mutex<Option<String>>>,
        transactions: Option<Arc<TransactionManager>>,
        portfolio: Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>>,
    }

    impl RecordingBridge {
        fn new(log: EventLog, symbol: Symbol) -> Self {
            Self {
                log,
                subscriptions: Arc::new(Mutex::new(vec![SubscriptionDataConfig::new_equity(
                    symbol,
                    Resolution::Minute,
                    DataNormalizationMode::Raw,
                )])),
                universe_changes: Arc::new(Mutex::new(Vec::new())),
                terminal_status_after_data: Arc::new(Mutex::new(None)),
                runtime_error_after_data: Arc::new(Mutex::new(None)),
                terminal_status: Arc::new(Mutex::new(None)),
                runtime_error: Arc::new(Mutex::new(None)),
                transactions: None,
                portfolio: None,
            }
        }

        fn with_universe_change(self, selection: UniverseSelection) -> Self {
            self.universe_changes.lock().unwrap().push(selection);
            self
        }

        fn with_terminal_status_after_data(self, status: AlgorithmStatus) -> Self {
            *self.terminal_status_after_data.lock().unwrap() = Some(status);
            self
        }

        fn with_runtime_error_after_data(self, message: &str) -> Self {
            *self.runtime_error_after_data.lock().unwrap() = Some(message.to_string());
            self
        }

        fn with_order_state(mut self, symbol: Symbol) -> Self {
            let transactions = Arc::new(TransactionManager::new());
            transactions.add_order(Order::market(1, symbol, dec!(10), DateTime::EPOCH, ""));
            self.transactions = Some(transactions);
            self.portfolio = Some(Arc::new(
                lean_algorithm::portfolio::SecurityPortfolioManager::new_live(dec!(100000)),
            ));
            self
        }
    }

    impl lean_algorithm::lifecycle::AlgorithmStateAccess for RecordingBridge {}

    impl lean_algorithm::lifecycle::LifecycleBridge for RecordingBridge {
        fn initialize(&mut self, _services: &mut dyn AlgorithmServices) -> anyhow::Result<()> {
            self.log.push("bridge:initialize");
            Ok(())
        }

        fn on_data(
            &mut self,
            _payload: DataDeliveryPayload,
            _services: &mut dyn AlgorithmServices,
        ) {
            self.log.push("bridge:on_data");
            if let Some(status) = *self.terminal_status_after_data.lock().unwrap() {
                *self.terminal_status.lock().unwrap() = Some(status);
            }
            if let Some(message) = self.runtime_error_after_data.lock().unwrap().clone() {
                *self.runtime_error.lock().unwrap() = Some(message);
            }
        }

        fn on_order_event(&mut self, _event: &OrderEvent, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_order_event");
        }

        fn on_otm_expiry(
            &mut self,
            _contract: lean_options::OptionContract,
            _quantity: rust_decimal::Decimal,
            _underlying_price: rust_decimal::Decimal,
            _entry_premium: rust_decimal::Decimal,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_assignment_order_event(
            &mut self,
            _contract: lean_options::OptionContract,
            _quantity: rust_decimal::Decimal,
            _is_assignment: bool,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_end_of_day(
            &mut self,
            _symbol: Option<Symbol>,
            _services: &mut dyn AlgorithmServices,
        ) {
            self.log.push("bridge:on_end_of_day");
        }

        fn on_warmup_finished(&mut self, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_warmup_finished");
        }

        fn on_end_of_algorithm(&mut self, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_end_of_algorithm");
        }

        fn on_margin_call(&mut self, _requests: &[Order], _services: &mut dyn AlgorithmServices) {}

        fn on_margin_call_warning(&mut self, _services: &mut dyn AlgorithmServices) {}

        fn on_securities_changed(
            &mut self,
            changes: &SecurityChanges,
            _services: &mut dyn AlgorithmServices,
        ) {
            self.log.push(format!(
                "bridge:on_securities_changed:{}:{}",
                changes.added.len(),
                changes.removed.len()
            ));
        }

        fn on_splits(
            &mut self,
            _splits: &HashMap<u64, lean_data::Split>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_dividends(
            &mut self,
            _dividends: &HashMap<u64, lean_data::Dividend>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_delistings(
            &mut self,
            _delistings: &HashMap<u64, lean_data::Delisting>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn on_symbol_changed_events(
            &mut self,
            _events: &HashMap<u64, lean_data::SymbolChangedEvent>,
            _services: &mut dyn AlgorithmServices,
        ) {
        }

        fn select_universe_changes(
            &mut self,
            _utc_ns: i64,
            _resolution: Resolution,
            _services: &mut dyn AlgorithmServices,
        ) -> Vec<UniverseSelection> {
            self.log.push("bridge:select_universe_changes");
            let selections: Vec<_> = self.universe_changes.lock().unwrap().drain(..).collect();
            if !selections.is_empty() {
                let mut subscriptions = self.subscriptions.lock().unwrap();
                for selection in &selections {
                    subscriptions.retain(|config| {
                        !selection
                            .changes
                            .removed
                            .iter()
                            .any(|symbol| symbol == &config.symbol)
                    });
                    for symbol in &selection.changes.added {
                        subscriptions.push(SubscriptionDataConfig::new_equity(
                            symbol.clone(),
                            selection.resolution,
                            DataNormalizationMode::Raw,
                        ));
                    }
                }
            }
            selections
        }

        fn select_custom_universe_changes(
            &mut self,
            _utc_ns: i64,
            _resolution: Resolution,
            _custom_data: &HashMap<String, Vec<lean_data::CustomDataPoint>>,
            _services: &mut dyn AlgorithmServices,
        ) -> Vec<UniverseSelection> {
            Vec::new()
        }

        fn on_end_of_time_step(&mut self, _services: &mut dyn AlgorithmServices) {
            self.log.push("bridge:on_end_of_time_step");
        }

        fn on_brokerage_message(&mut self, _message: &str, _services: &mut dyn AlgorithmServices) {}

        fn on_brokerage_disconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

        fn on_brokerage_reconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

        fn terminal_status(&self) -> Option<AlgorithmStatus> {
            *self.terminal_status.lock().unwrap()
        }

        fn runtime_error(&self) -> Option<String> {
            self.runtime_error.lock().unwrap().clone()
        }

        fn name(&self) -> &str {
            "RecordingBridge"
        }

        fn start_date(&self) -> DateTime {
            DateTime::EPOCH
        }

        fn end_date(&self) -> DateTime {
            DateTime::from(Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0).unwrap())
        }

        fn portfolio_value(&self) -> lean_core::Price {
            dec!(100000)
        }

        fn starting_cash(&self) -> lean_core::Price {
            dec!(100000)
        }

        fn subscriptions(&self) -> Vec<SubscriptionDataConfig> {
            self.subscriptions.lock().unwrap().clone()
        }

        fn prepare_data_delivery(
            &mut self,
            _subscriptions: &[SubscriptionDataConfig],
        ) -> anyhow::Result<()> {
            self.log.push("bridge:prepare_data_delivery");
            Ok(())
        }

        fn option_subscriptions(&self) -> Vec<OptionSubscription> {
            Vec::new()
        }

        fn portfolio(&self) -> Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>> {
            self.portfolio.clone()
        }

        fn transactions(&self) -> Option<Arc<TransactionManager>> {
            self.transactions.clone()
        }

        fn order_fee(&self, _order: &Order, _fill_price: lean_core::Price) -> lean_core::Price {
            dec!(0)
        }

        fn contract_multiplier_for_symbol(&self, _symbol: &Symbol) -> lean_core::Price {
            dec!(1)
        }

        fn validate_order_buying_power(
            &self,
            _order: &Order,
            _fill_price: lean_core::Price,
            _fee: lean_core::Price,
        ) -> std::result::Result<(), String> {
            Ok(())
        }

        fn has_universes(&self) -> bool {
            !self.universe_changes.lock().unwrap().is_empty()
        }

        fn universe_resolution(&self) -> Option<Resolution> {
            Some(Resolution::Minute)
        }
    }

    impl DataQueueHandler for RecordingLiveHandler {
        fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
            self.calls.lock().unwrap().push(self.name);
            if !self.accepts {
                return Err(LeanError::Unsupported(format!(
                    "{} does not support {}",
                    self.name, config.symbol
                )));
            }

            let (sender, receiver) = live_data_channel();
            sender
                .send(Ok(LiveDataItem::Heartbeat(lean_core::DateTime::EPOCH)))
                .unwrap();
            Ok(LiveDataSubscription::new(
                LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
                receiver,
            ))
        }

        fn unsubscribe(&mut self, _config: &SubscriptionDataConfig) -> Result<()> {
            Ok(())
        }

        fn is_connected(&self) -> bool {
            true
        }

        fn name(&self) -> &str {
            self.name
        }
    }

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    fn spy() -> Symbol {
        Symbol::create_equity("SPY", &Market::usa())
    }

    fn qqq() -> Symbol {
        Symbol::create_equity("QQQ", &Market::usa())
    }

    fn trade_bar_item(symbol: Symbol, date: NaiveDate, minute: u32) -> LiveDataItem {
        LiveDataItem::TradeBar(TradeBar::new(
            symbol,
            dt(date, 9, 30 + minute),
            TimeSpan::from_mins(1),
            TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
        ))
    }

    fn emit_ready(items: Vec<LiveDataItem>) -> Vec<LiveDataItem> {
        let next_time = items
            .iter()
            .map(LiveDataItem::end_time)
            .max()
            .unwrap_or(DateTime::EPOCH)
            + TimeSpan::from_mins(1);
        let mut out = items;
        out.push(LiveDataItem::Heartbeat(next_time));
        out
    }

    async fn live_config(log: EventLog, items: Vec<LiveDataItem>) -> LiveRunConfig {
        let tmp = tempfile::tempdir().unwrap().keep();
        let store = Arc::new(
            lean_storage::IcebergStore::connect_local(&tmp)
                .await
                .unwrap(),
        );
        LiveRunConfig {
            data_root: tmp,
            data_store: store,
            history_provider: None,
            parameters: HashMap::new(),
            custom_data_sources: Vec::new(),
            live_data_queue: lean_live::DataQueueHandlerManager::new(vec![Box::new(
                ScriptedLiveHandler { log, items },
            )]),
            brokerage: None,
            brokerage_model: None,
            paper_trading: true,
            max_slices: Some(1),
            max_runtime: Some(Duration::from_millis(50)),
            output_dir: None,
        }
    }

    #[test]
    fn live_subscription_set_uses_stacked_queue_provider_order() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut queue = lean_live::DataQueueHandlerManager::new(vec![
            Box::new(RecordingLiveHandler {
                name: "tradier",
                accepts: false,
                calls: calls.clone(),
            }),
            Box::new(RecordingLiveHandler {
                name: "thetadata",
                accepts: true,
                calls: calls.clone(),
            }),
        ]);
        let config = SubscriptionDataConfig::new_crypto(
            Symbol::create_crypto("BTCUSDT", &Market::binance()),
            Resolution::Minute,
        );

        let set = LiveSubscriptionSet::subscribe_initial(
            &mut queue,
            &[config],
            Some("paper".to_string()),
            true,
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(set.market.len(), 1);
        assert_eq!(calls.lock().unwrap().as_slice(), &["tradier", "thetadata"]);
    }

    #[tokio::test]
    async fn live_run_invokes_initialize_prepare_warmup_before_subscribing() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert!(log.position("bridge:initialize") < log.position("bridge:prepare_data_delivery"));
        assert!(
            log.position("bridge:prepare_data_delivery")
                < log.position("bridge:on_warmup_finished")
        );
        assert!(log.position("bridge:on_warmup_finished") < log.position("queue:set_job"));
        assert!(log.position("queue:set_job") < log.position("queue:subscribe:SPY"));
    }

    #[tokio::test]
    async fn live_run_delivers_on_data_then_framework_then_end_time_step_per_slice() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_time_step"));
    }

    #[tokio::test]
    async fn live_run_calls_warmup_finished_once_before_first_data() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("bridge:on_warmup_finished"), 1);
        assert!(log.position("bridge:on_warmup_finished") < log.position("bridge:on_data"));
    }

    #[tokio::test]
    async fn live_run_calls_end_of_day_on_day_transition_and_final_finish() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let first_day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let second_day = NaiveDate::from_ymd_opt(2024, 1, 18).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), first_day, 0),
                trade_bar_item(symbol, second_day, 0),
            ]),
        )
        .await;
        config.max_slices = Some(2);

        run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("bridge:on_end_of_day"), 2);
        assert!(log.position("bridge:on_end_of_day") < log.position("bridge:on_end_of_algorithm"));
    }

    #[tokio::test]
    async fn live_run_calls_on_end_of_algorithm_even_when_stopped_by_max_slices() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), day, 0),
                trade_bar_item(symbol, day, 1),
            ]),
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_algorithm"));
        assert!(log.position("bridge:on_end_of_algorithm") < log.position("queue:unsubscribe:SPY"));
    }

    #[tokio::test]
    async fn live_run_unsubscribes_all_after_finish() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("queue:unsubscribe:SPY"), 1);
        assert!(log.position("bridge:on_end_of_algorithm") < log.position("queue:unsubscribe:SPY"));
    }

    #[tokio::test]
    async fn live_run_syncs_dynamic_subscriptions_after_universe_changes() {
        let log = EventLog::default();
        let symbol = spy();
        let added = qqq();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone()).with_universe_change(
            UniverseSelection {
                changes: SecurityChanges {
                    added: vec![added.clone()],
                    removed: Vec::new(),
                },
                resolution: Resolution::Minute,
            },
        );
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol.clone(), day, 0)]),
        )
        .await;

        run_live(bridge, config).await.unwrap();

        assert!(
            log.position("bridge:on_securities_changed:1:0") < log.position("queue:subscribe:QQQ")
        );
        assert_eq!(log.count("queue:subscribe:QQQ"), 1);
        assert_eq!(log.count("queue:unsubscribe:QQQ"), 1);
    }

    #[tokio::test]
    async fn live_run_processes_order_events_before_on_data() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge =
            RecordingBridge::new(log.clone(), symbol.clone()).with_order_state(symbol.clone());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = live_config(
            log.clone(),
            emit_ready(vec![trade_bar_item(symbol, day, 0)]),
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(log.count("bridge:on_order_event"), 1);
        assert!(log.position("bridge:on_order_event") < log.position("bridge:on_data"));
        assert_eq!(result.order_events.len(), 1);
    }

    #[tokio::test]
    async fn live_run_ignores_empty_slices_without_callbacks() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol);
        let config = live_config(
            log.clone(),
            vec![
                LiveDataItem::Heartbeat(DateTime::EPOCH),
                LiveDataItem::Heartbeat(DateTime::EPOCH + TimeSpan::from_mins(1)),
            ],
        )
        .await;

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 0);
        assert_eq!(log.count("bridge:on_data"), 0);
        assert_eq!(log.count("bridge:on_framework_data"), 0);
        assert_eq!(log.count("bridge:on_end_of_time_step"), 0);
    }

    #[tokio::test]
    async fn live_run_stops_on_algorithm_terminal_status() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone())
            .with_terminal_status_after_data(AlgorithmStatus::Stopped);
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), day, 0),
                trade_bar_item(symbol, day, 1),
            ]),
        )
        .await;
        config.max_slices = Some(10);

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert_eq!(log.count("bridge:on_data"), 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_algorithm"));
    }

    #[tokio::test]
    async fn live_run_stops_on_runtime_error_handler_signal() {
        let log = EventLog::default();
        let symbol = spy();
        let bridge = RecordingBridge::new(log.clone(), symbol.clone())
            .with_runtime_error_after_data("fatal result handler error");
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let mut config = live_config(
            log.clone(),
            emit_ready(vec![
                trade_bar_item(symbol.clone(), day, 0),
                trade_bar_item(symbol, day, 1),
            ]),
        )
        .await;
        config.max_slices = Some(10);

        let result = run_live(bridge, config).await.unwrap();

        assert_eq!(result.slices_processed, 1);
        assert_eq!(log.count("bridge:on_data"), 1);
        assert!(log.position("bridge:on_data") < log.position("bridge:on_end_of_algorithm"));
    }
}
