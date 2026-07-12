use anyhow::Result;
use chrono::{NaiveDate, TimeZone, Utc};
use lean_algorithm::algorithm::{DataDeliveryPayload, SecurityChanges};
use lean_algorithm::lifecycle::{
    AlgorithmBridge, AlgorithmServices, AlgorithmStateAccess, OptionSubscription, UniverseSelection,
};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{
    DataNormalizationMode, DateTime, Market, Price, Resolution, SecurityType, Symbol, TickType,
};
use lean_data::{
    split::SplitType, CustomDataPoint, Delisting, DelistingType, Dividend, Slice, Split,
    SubscriptionDataConfig, SymbolChangedEvent, TradeBar, TradeBarData,
};
use lean_data_providers::{DataType, HistoryRequest, IHistoryProvider};
use lean_engine::{
    algorithm_manager::{AlgorithmManager, OrderEventProcessing},
    AlgorithmRuntimeContext,
};
use lean_engine::{runner::backtest::run_backtest, BacktestRunConfig};
use lean_orders::{
    fill_model::ImmediateFillModel, order_processor::OrderProcessor, slippage::NullSlippageModel,
};
use lean_orders::{Order, OrderEvent, TransactionManager};
use lean_statistics::TradeBuilder;
use lean_storage::IcebergStore;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

fn test_runtime_context() -> AlgorithmRuntimeContext {
    AlgorithmRuntimeContext::with_history_service(
        Arc::new(lean_algorithm::lifecycle::NullHistoryService),
        HashMap::new(),
    )
}

fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
}

struct OneBarHistoryProvider {
    bar: TradeBar,
}

#[async_trait::async_trait]
impl IHistoryProvider for OneBarHistoryProvider {
    async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
        if request.data_type == DataType::TradeBar
            && self.bar.symbol.id.sid == request.symbol.id.sid
        {
            Ok(vec![self.bar.clone()])
        } else {
            Ok(Vec::new())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleEvent {
    Initialize,
    PrepareDataDelivery,
    OnSecuritiesChanged,
    OnSplits,
    OnDividends,
    OnDelistings,
    OnSymbolChangedEvents,
    OnData,
    OnEndOfTimeStep,
    OnOrderEvent,
    OnEndOfDay,
    OnWarmupFinished,
    OnEndOfAlgorithm,
}

#[derive(Clone)]
struct RecordingBacktestAlgorithm {
    algorithm: Arc<Mutex<QcAlgorithm>>,
    events: Arc<Mutex<Vec<LifecycleEvent>>>,
    universe_symbol: Symbol,
    universe_selection_emitted: bool,
    securities_changed: Arc<Mutex<Option<SecurityChanges>>>,
    on_data_warmup_states: Arc<Mutex<Vec<bool>>>,
    on_data_times: Arc<Mutex<Vec<DateTime>>>,
}

impl RecordingBacktestAlgorithm {
    fn new(symbol: Symbol, universe_symbol: Symbol) -> Self {
        let mut algorithm = QcAlgorithm::new("recording-lifecycle", dec!(100_000));
        algorithm.set_start_date(2024, 1, 2);
        algorithm.set_end_date(2024, 1, 2);
        algorithm.add_equity_with_normalization(
            symbol.value.as_ref(),
            Resolution::Daily,
            Some(DataNormalizationMode::Raw),
        );
        Self {
            algorithm: Arc::new(Mutex::new(algorithm)),
            events: Arc::new(Mutex::new(Vec::new())),
            universe_symbol,
            universe_selection_emitted: false,
            securities_changed: Arc::new(Mutex::new(None)),
            on_data_warmup_states: Arc::new(Mutex::new(Vec::new())),
            on_data_times: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn record(&self, event: LifecycleEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl AlgorithmStateAccess for RecordingBacktestAlgorithm {
    fn algorithm_state(&self) -> Option<Arc<Mutex<QcAlgorithm>>> {
        Some(self.algorithm.clone())
    }
}

impl AlgorithmBridge for RecordingBacktestAlgorithm {
    fn initialize(&mut self, _services: &mut dyn AlgorithmServices) -> Result<()> {
        self.record(LifecycleEvent::Initialize);
        Ok(())
    }

    fn on_data(&mut self, payload: DataDeliveryPayload, _services: &mut dyn AlgorithmServices) {
        assert!(payload.slice.has_data);
        self.on_data_warmup_states
            .lock()
            .unwrap()
            .push(self.algorithm.lock().unwrap().is_warming_up);
        self.on_data_times.lock().unwrap().push(payload.slice.time);
        self.record(LifecycleEvent::OnData);
    }

    fn on_order_event(&mut self, _event: &OrderEvent, _services: &mut dyn AlgorithmServices) {
        self.record(LifecycleEvent::OnOrderEvent);
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

    fn on_end_of_day(&mut self, _symbol: Option<Symbol>, _services: &mut dyn AlgorithmServices) {
        self.record(LifecycleEvent::OnEndOfDay);
    }

    fn on_warmup_finished(&mut self, _services: &mut dyn AlgorithmServices) {
        self.record(LifecycleEvent::OnWarmupFinished);
    }

    fn on_end_of_algorithm(&mut self, _services: &mut dyn AlgorithmServices) {
        self.record(LifecycleEvent::OnEndOfAlgorithm);
    }

    fn on_margin_call(&mut self, _requests: &[Order], _services: &mut dyn AlgorithmServices) {}

    fn on_margin_call_warning(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn on_securities_changed(
        &mut self,
        changes: &SecurityChanges,
        _services: &mut dyn AlgorithmServices,
    ) {
        assert_eq!(changes.added, vec![self.universe_symbol.clone()]);
        assert!(changes.removed.is_empty());
        *self.securities_changed.lock().unwrap() = Some(changes.clone());
        self.record(LifecycleEvent::OnSecuritiesChanged);
    }

    fn on_splits(&mut self, splits: &HashMap<u64, Split>, _services: &mut dyn AlgorithmServices) {
        assert_eq!(splits.len(), 1);
        self.record(LifecycleEvent::OnSplits);
    }

    fn on_dividends(
        &mut self,
        dividends: &HashMap<u64, Dividend>,
        _services: &mut dyn AlgorithmServices,
    ) {
        assert_eq!(dividends.len(), 1);
        self.record(LifecycleEvent::OnDividends);
    }

    fn on_delistings(
        &mut self,
        delistings: &HashMap<u64, Delisting>,
        _services: &mut dyn AlgorithmServices,
    ) {
        assert_eq!(delistings.len(), 1);
        self.record(LifecycleEvent::OnDelistings);
    }

    fn on_symbol_changed_events(
        &mut self,
        events: &HashMap<u64, SymbolChangedEvent>,
        _services: &mut dyn AlgorithmServices,
    ) {
        assert_eq!(events.len(), 1);
        self.record(LifecycleEvent::OnSymbolChangedEvents);
    }

    fn select_universe_changes(
        &mut self,
        _utc_ns: i64,
        _resolution: Resolution,
        _services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        if self.universe_selection_emitted {
            return Vec::new();
        }
        self.universe_selection_emitted = true;
        vec![UniverseSelection {
            changes: SecurityChanges {
                added: vec![self.universe_symbol.clone()],
                removed: Vec::new(),
            },
            resolution: Resolution::Daily,
        }]
    }

    fn select_custom_universe_changes(
        &mut self,
        _utc_ns: i64,
        _resolution: Resolution,
        _custom_data: &HashMap<String, Vec<CustomDataPoint>>,
        _services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        Vec::new()
    }

    fn on_end_of_time_step(&mut self, _services: &mut dyn AlgorithmServices) {
        self.record(LifecycleEvent::OnEndOfTimeStep);
    }

    fn on_brokerage_message(&mut self, message: &str, _services: &mut dyn AlgorithmServices) {
        assert!(!message.is_empty());
    }

    fn on_brokerage_disconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_reconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn name(&self) -> &str {
        "recording-lifecycle"
    }

    fn start_date(&self) -> DateTime {
        self.algorithm.lock().unwrap().start_date
    }

    fn end_date(&self) -> DateTime {
        self.algorithm.lock().unwrap().end_date
    }

    fn portfolio_value(&self) -> Price {
        self.algorithm.lock().unwrap().portfolio_value()
    }

    fn starting_cash(&self) -> Price {
        self.algorithm.lock().unwrap().portfolio.starting_cash()
    }

    fn subscriptions(&self) -> Vec<Arc<SubscriptionDataConfig>> {
        self.algorithm
            .lock()
            .unwrap()
            .subscription_manager
            .get_all()
    }

    fn subscriptions_version(&self) -> u64 {
        let algorithm = self.algorithm.lock().unwrap();
        algorithm
            .subscription_manager
            .generation()
            .wrapping_add(algorithm.option_subscriptions_generation)
    }

    fn prepare_data_delivery(&mut self, subscriptions: &[SubscriptionDataConfig]) -> Result<()> {
        assert_eq!(subscriptions.len(), 1);
        self.record(LifecycleEvent::PrepareDataDelivery);
        Ok(())
    }

    fn option_subscriptions(&self) -> Vec<OptionSubscription> {
        Vec::new()
    }

    fn portfolio(&self) -> Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>> {
        Some(self.algorithm.lock().unwrap().portfolio.clone())
    }

    fn transactions(&self) -> Option<Arc<TransactionManager>> {
        Some(self.algorithm.lock().unwrap().transactions.clone())
    }

    fn order_fee(&self, order: &Order, fill_price: Price) -> Price {
        self.algorithm
            .lock()
            .unwrap()
            .order_fee(order, fill_price)
            .amount
    }

    fn contract_multiplier_for_symbol(&self, symbol: &Symbol) -> Price {
        self.algorithm
            .lock()
            .unwrap()
            .contract_multiplier_for_symbol(symbol)
    }

    fn validate_order_buying_power(
        &self,
        order: &Order,
        fill_price: Price,
        fee: Price,
    ) -> std::result::Result<(), String> {
        self.algorithm
            .lock()
            .unwrap()
            .validate_order_buying_power(order, fill_price, fee)
    }

    fn has_universes(&self) -> bool {
        true
    }

    fn universe_resolution(&self) -> Option<Resolution> {
        Some(Resolution::Daily)
    }
}

#[tokio::test]
async fn backtest_runner_delivers_lifecycle_callbacks_to_sdk_bridge() {
    // Mirrors the C# LEAN AlgorithmManagerTests style: run the real manager loop
    // against a small data feed and assert lifecycle callbacks reach the
    // algorithm boundary in order.
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        IcebergStore::connect_local(tmp.path().to_path_buf())
            .await
            .unwrap(),
    );
    let symbol = Symbol::create_equity("SPY", &Market::usa());
    let universe_symbol = Symbol::create_equity("AAPL", &Market::usa());
    let algorithm = RecordingBacktestAlgorithm::new(symbol, universe_symbol.clone());
    let subscription_symbol = algorithm.subscriptions()[0].symbol.clone();
    let algorithm_state = algorithm.algorithm.clone();
    let securities_changed = algorithm.securities_changed.clone();
    let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
    let bar = TradeBar::new(
        subscription_symbol,
        DateTime::from(date.and_hms_opt(0, 0, 0).unwrap()),
        lean_core::TimeSpan::ONE_DAY,
        TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1_000)),
    );
    let events = algorithm.events.clone();
    let result = run_backtest(
        algorithm,
        BacktestRunConfig {
            data_root: tmp.path().to_path_buf(),
            data_store: store,
            history_provider: Some(Arc::new(OneBarHistoryProvider { bar })),
            start_date_override: Some(date),
            end_date_override: Some(date),
            ..BacktestRunConfig::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(result.trading_days, 1);
    assert_eq!(
        securities_changed.lock().unwrap().as_ref().unwrap().added,
        vec![universe_symbol.clone()]
    );
    assert!(
        algorithm_state
            .lock()
            .unwrap()
            .securities
            .contains(&universe_symbol),
        "universe-selected symbol should be added to QcAlgorithm securities before callback"
    );
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            LifecycleEvent::Initialize,
            LifecycleEvent::PrepareDataDelivery,
            LifecycleEvent::OnWarmupFinished,
            LifecycleEvent::OnSecuritiesChanged,
            LifecycleEvent::OnData,
            LifecycleEvent::OnEndOfTimeStep,
            LifecycleEvent::OnEndOfDay,
            LifecycleEvent::OnEndOfAlgorithm,
        ]
    );
}

#[tokio::test]
async fn backtest_runner_replays_warmup_before_warmup_finished() {
    let tmp = tempfile::tempdir().unwrap();
    let store = Arc::new(
        IcebergStore::connect_local(tmp.path().to_path_buf())
            .await
            .unwrap(),
    );
    let symbol = Symbol::create_equity("SPY", &Market::usa());
    let universe_symbol = Symbol::create_equity("AAPL", &Market::usa());
    let algorithm = RecordingBacktestAlgorithm::new(symbol, universe_symbol);
    algorithm
        .algorithm
        .lock()
        .unwrap()
        .set_warm_up(lean_core::TimeSpan::ONE_DAY);
    let subscription_symbol = algorithm.subscriptions()[0].symbol.clone();
    let algorithm_state = algorithm.algorithm.clone();
    let on_data_warmup_states = algorithm.on_data_warmup_states.clone();
    let on_data_times = algorithm.on_data_times.clone();
    let events = algorithm.events.clone();
    let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 3).unwrap();
    let warmup_date = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
    let warmup_bar = TradeBar::new(
        subscription_symbol.clone(),
        dt(warmup_date, 16, 0),
        lean_core::TimeSpan::ONE_DAY,
        TradeBarData::new(dec!(90), dec!(91), dec!(89), dec!(90), dec!(1_000)),
    );
    let normal_bar = TradeBar::new(
        subscription_symbol,
        dt(date, 16, 0),
        lean_core::TimeSpan::ONE_DAY,
        TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1_000)),
    );
    store
        .append_trade_bars(
            &[warmup_bar, normal_bar],
            SecurityType::Equity,
            Market::usa().as_str(),
            Resolution::Daily,
            TickType::Trade,
        )
        .await
        .unwrap();

    run_backtest(
        algorithm,
        BacktestRunConfig {
            data_root: tmp.path().to_path_buf(),
            data_store: store,
            start_date_override: Some(date),
            end_date_override: Some(date),
            ..BacktestRunConfig::default()
        },
    )
    .await
    .unwrap();

    assert!(
        !algorithm_state.lock().unwrap().is_warming_up,
        "warmup state should be cleared before normal data delivery"
    );
    let events = events.lock().unwrap();
    let warmup_finished = events
        .iter()
        .position(|event| *event == LifecycleEvent::OnWarmupFinished)
        .expect("warmup finished callback");
    let on_data_positions = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| (*event == LifecycleEvent::OnData).then_some(index))
        .collect::<Vec<_>>();
    assert_eq!(
        on_data_positions.len(),
        2,
        "warmup and normal OnData callbacks should both run; times={:?}, warmup_states={:?}, events={:?}",
        on_data_times.lock().unwrap().as_slice(),
        on_data_warmup_states.lock().unwrap().as_slice(),
        events.as_slice()
    );
    assert!(
        on_data_positions[0] < warmup_finished && warmup_finished < on_data_positions[1],
        "warmup data must replay before OnWarmupFinished, then normal OnData follows"
    );
    assert_eq!(
        on_data_warmup_states.lock().unwrap().as_slice(),
        &[true, false],
        "first OnData should be during warmup and second should be normal delivery"
    );
}

#[test]
fn algorithm_manager_dispatches_auxiliary_slice_callbacks_before_on_data() {
    let symbol = Symbol::create_equity("SPY", &Market::usa());
    let universe_symbol = Symbol::create_equity("AAPL", &Market::usa());
    let algorithm = RecordingBacktestAlgorithm::new(symbol.clone(), universe_symbol);
    let events = algorithm.events.clone();
    let mut manager = AlgorithmManager::new(algorithm, test_runtime_context());
    let mut services = lean_algorithm::NoopAlgorithmServices::default();
    let time = DateTime::from_secs(1_704_153_600);

    let mut slice = Slice::new(time);
    slice.add_split(Split::new(
        symbol.clone(),
        time,
        dec!(0.5),
        dec!(100),
        SplitType::SplitOccurred,
    ));
    slice.add_dividend(Dividend::new(symbol.clone(), time, dec!(1), dec!(100)));
    slice.add_delisting(Delisting::new(
        symbol.clone(),
        time,
        dec!(0),
        DelistingType::Warning,
    ));
    slice.add_symbol_changed(SymbolChangedEvent::new(
        symbol,
        time,
        "OLD".to_string(),
        "SPY".to_string(),
    ));

    manager.deliver_data(
        DataDeliveryPayload {
            slice: Arc::new(slice),
        },
        &mut services,
    );

    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            LifecycleEvent::OnSplits,
            LifecycleEvent::OnDividends,
            LifecycleEvent::OnDelistings,
            LifecycleEvent::OnSymbolChangedEvents,
            LifecycleEvent::OnData,
        ]
    );
}

#[test]
fn algorithm_manager_calls_end_of_day_on_day_change_and_finish() {
    // Mirrors LEAN AlgorithmManagerTests coverage for day-boundary lifecycle
    // callbacks: previous-day EOD fires when a new trading day is observed, and
    // finish emits a final EOD before OnEndOfAlgorithm.
    let symbol = Symbol::create_equity("SPY", &Market::usa());
    let universe_symbol = Symbol::create_equity("AAPL", &Market::usa());
    let algorithm = RecordingBacktestAlgorithm::new(symbol, universe_symbol);
    let events = algorithm.events.clone();
    let mut manager = AlgorithmManager::new(algorithm, test_runtime_context());
    let mut services = lean_algorithm::NoopAlgorithmServices::default();

    let day1 = DateTime::from_secs(1_704_153_600);
    let day2 = DateTime::from_secs(1_704_240_000);
    assert!(manager.handle_new_trading_day(&Slice::new(day1), &mut services));
    assert!(!manager.handle_new_trading_day(&Slice::new(day1), &mut services));
    assert!(manager.handle_new_trading_day(&Slice::new(day2), &mut services));
    manager.finish(&mut services);

    assert_eq!(manager.trading_days(), 2);
    assert_eq!(manager.slices_processed(), 3);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            LifecycleEvent::OnEndOfDay,
            LifecycleEvent::OnEndOfDay,
            LifecycleEvent::OnEndOfAlgorithm,
        ]
    );
}

#[test]
fn algorithm_manager_dispatches_order_event_after_fill_settlement() {
    // Mirrors LEAN BacktestingTransactionHandlerTests coverage that filled
    // orders flow back through OnOrderEvent during the engine loop.
    let symbol = Symbol::create_equity("SPY", &Market::usa());
    let universe_symbol = Symbol::create_equity("AAPL", &Market::usa());
    let algorithm = RecordingBacktestAlgorithm::new(symbol.clone(), universe_symbol);
    let events = algorithm.events.clone();
    let portfolio = algorithm.portfolio().unwrap();
    let transaction_manager = Arc::new(TransactionManager::new());
    transaction_manager.add_order(Order::market(
        1,
        symbol.clone(),
        dec!(10),
        DateTime::from_secs(0),
        "entry",
    ));
    let processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        transaction_manager,
    );
    let mut manager = AlgorithmManager::new(algorithm, test_runtime_context());
    let mut services = lean_algorithm::NoopAlgorithmServices::default();
    let time = DateTime::from_secs(60);
    let mut slice = Slice::new(time);
    slice.add_bar(TradeBar::new(
        symbol.clone(),
        time,
        lean_core::TimeSpan::ONE_MINUTE,
        TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1_000)),
    ));
    let mut all_order_events = Vec::new();
    let mut trade_builder = TradeBuilder::new();
    let mut completed_trades = Vec::new();

    manager.process_order_events(OrderEventProcessing {
        slice: &slice,
        option_chains: &[],
        order_processor: Some(&processor),
        portfolio: Some(&portfolio),
        services: &mut services,
        all_order_events: &mut all_order_events,
        trade_builder: &mut trade_builder,
        completed_trades: &mut completed_trades,
    });

    assert_eq!(all_order_events.len(), 1);
    assert_eq!(all_order_events[0].fill_quantity, dec!(10));
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[LifecycleEvent::OnOrderEvent]
    );
}
