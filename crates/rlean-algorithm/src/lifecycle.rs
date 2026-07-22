use crate::algorithm::{AlgorithmStatus, DataDeliveryPayload, SecurityChanges};
use crate::charting::ChartCollection;
use crate::framework_registry::FrameworkModelRegistry;
use crate::qc_algorithm::{OptionFilter, QcAlgorithm};
use rlean_alpha::AlphaAnalytics;
use rlean_core::{DateTime, Price, Resolution, Symbol};
use rlean_data::{
    Delisting, Dividend, FundamentalData, Split, SubscriptionDataConfig, SymbolChangedEvent,
};
use rlean_data_tables::CustomDataPoint;
use rlean_options::OptionContract;
use rlean_orders::{Order, OrderEvent, TransactionManager};
use rust_decimal::Decimal;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub struct ScheduledEventRegistrationRequest {
    pub name: String,
    pub date_rule: rlean_scheduling::DateRule,
    pub time_rule: rlean_scheduling::TimeRule,
    pub callback: Box<dyn FnMut() -> Result<(), String> + Send>,
}

pub type HistoryColumns = HashMap<String, Vec<String>>;

pub trait AlgorithmHistoryService: Send + Sync {
    fn history(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        periods: usize,
        resolution: Resolution,
    ) -> HistoryColumns;

    fn last_known_close_price(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        resolution: Resolution,
    ) -> Option<f64> {
        let history = self.history(algorithm, symbol, 1, resolution);
        ["close", "price", "ask_close", "bid_close"]
            .iter()
            .filter_map(|column| history.get(*column))
            .flat_map(|values| values.iter().rev())
            .filter_map(|value| value.parse::<f64>().ok())
            .find(|value| *value > 0.0 && value.is_finite())
    }
}

pub trait RegisteredIndicatorBridge: Send + Sync {
    fn update_bar(&self, bar: &rlean_data_tables::TradeBar) -> bool;
}

pub type RegisteredIndicatorRegistry =
    Arc<Mutex<HashMap<u64, Vec<Arc<dyn RegisteredIndicatorBridge>>>>>;

pub type CustomUniverseSelectFn = Arc<dyn Fn(&[CustomDataPoint]) -> Vec<Symbol> + Send + Sync>;
pub type FundamentalUniverseSelectFn = Arc<dyn Fn(&[FundamentalData]) -> Vec<Symbol> + Send + Sync>;

pub struct CustomUniverseSelectorRegistrationRequest {
    pub source_type: String,
    pub ticker: String,
    pub resolution: Resolution,
    pub settings_resolution: Resolution,
    pub minimum_time_in_universe_secs: f64,
    pub select: CustomUniverseSelectFn,
}

pub struct FundamentalUniverseSelectorRegistrationRequest {
    pub source_type: String,
    pub resolution: Resolution,
    pub settings_resolution: Resolution,
    pub minimum_time_in_universe_secs: f64,
    pub select: FundamentalUniverseSelectFn,
}

pub trait AlgorithmRuntimeServices: Send + Sync {
    fn history_service(&self) -> Arc<dyn AlgorithmHistoryService>;
    fn runtime_parameters(&self) -> Arc<std::sync::RwLock<HashMap<String, String>>>;
    fn registered_indicators(&self) -> RegisteredIndicatorRegistry;
    fn register_scheduled_event(&self, registration: ScheduledEventRegistrationRequest) {
        let _ = registration;
    }
    fn framework_state(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
    fn framework_registry(&self) -> Option<Arc<dyn FrameworkModelRegistry>> {
        None
    }
    fn custom_universe_registry(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        None
    }
    fn register_custom_universe_selector(
        &self,
        registration: CustomUniverseSelectorRegistrationRequest,
    ) {
        let _ = registration;
    }
    fn run_custom_universe_selections(
        &self,
        utc_ns: i64,
        resolution: Resolution,
        custom_data: &HashMap<String, Vec<CustomDataPoint>>,
    ) -> Vec<UniverseSelection> {
        let _ = (utc_ns, resolution, custom_data);
        Vec::new()
    }
    fn has_custom_universe_selectors(&self) -> bool {
        false
    }
    fn custom_universe_selector_resolution(&self) -> Option<Resolution> {
        None
    }
    fn register_fundamental_universe_selector(
        &self,
        registration: FundamentalUniverseSelectorRegistrationRequest,
    ) {
        let _ = registration;
    }
    fn run_fundamental_universe_selections(
        &self,
        utc_ns: i64,
        resolution: Resolution,
        fundamentals: &[FundamentalData],
    ) -> Vec<UniverseSelection> {
        let _ = (utc_ns, resolution, fundamentals);
        Vec::new()
    }
    fn has_fundamental_universe_selectors(&self) -> bool {
        false
    }
    fn fundamental_universe_selector_resolution(&self) -> Option<Resolution> {
        None
    }
}

#[derive(Debug, Default)]
pub struct NullHistoryService;

impl AlgorithmHistoryService for NullHistoryService {
    fn history(
        &self,
        _algorithm: &QcAlgorithm,
        _symbol: &Symbol,
        _periods: usize,
        _resolution: Resolution,
    ) -> HistoryColumns {
        HistoryColumns::new()
    }
}

/// Language-neutral services exposed to strategy adapters.
///
/// `rlean-engine` implements this surface while it owns runner, data feed,
/// orders, and results. Language bindings communicate through this interface
/// rather than owning execution loops.
pub trait AlgorithmServices {
    fn time(&self) -> DateTime;
    fn set_runtime_parameter(&mut self, key: &str, value: String);
    fn runtime_parameter(&self, key: &str) -> Option<String>;
    fn emit_debug(&mut self, message: &str);
    fn history_service(&self) -> Arc<dyn AlgorithmHistoryService>;
    fn runtime_services(&self) -> Option<Arc<dyn AlgorithmRuntimeServices>> {
        None
    }
}

pub struct NoopAlgorithmServices {
    time: DateTime,
    parameters: HashMap<String, String>,
}

impl NoopAlgorithmServices {
    pub fn new(time: DateTime, parameters: HashMap<String, String>) -> Self {
        Self { time, parameters }
    }
}

impl AlgorithmServices for NoopAlgorithmServices {
    fn time(&self) -> DateTime {
        self.time
    }

    fn set_runtime_parameter(&mut self, key: &str, value: String) {
        self.parameters.insert(key.to_string(), value);
    }

    fn runtime_parameter(&self, key: &str) -> Option<String> {
        self.parameters.get(key).cloned()
    }

    fn emit_debug(&mut self, message: &str) {
        tracing::debug!("{message}");
    }

    fn history_service(&self) -> Arc<dyn AlgorithmHistoryService> {
        Arc::new(NullHistoryService)
    }
}

impl Default for NoopAlgorithmServices {
    fn default() -> Self {
        Self {
            time: DateTime::now(),
            parameters: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OptionSubscription {
    pub canonical: Symbol,
    pub resolution: Resolution,
    pub filter: OptionFilter,
}

#[derive(Debug, Clone)]
pub struct UniverseSelection {
    pub changes: SecurityChanges,
    pub resolution: Resolution,
}

/// Engine-only access to canonical algorithm state.
pub trait AlgorithmStateAccess {
    fn algorithm_state(&self) -> Option<Arc<Mutex<QcAlgorithm>>> {
        None
    }
}

/// Universal lifecycle callback boundary consumed by `rlean-engine`.
///
/// All methods here are required. A bridge that cannot implement a LEAN
/// lifecycle callback should fail to compile instead of silently swallowing it.
pub trait LifecycleBridge: Send + AlgorithmStateAccess {
    fn initialize(&mut self, services: &mut dyn AlgorithmServices) -> anyhow::Result<()>;
    fn on_data(&mut self, payload: DataDeliveryPayload, services: &mut dyn AlgorithmServices);
    fn on_order_event(&mut self, event: &OrderEvent, services: &mut dyn AlgorithmServices);
    fn on_otm_expiry(
        &mut self,
        contract: OptionContract,
        quantity: Decimal,
        underlying_price: Decimal,
        entry_premium: Decimal,
        services: &mut dyn AlgorithmServices,
    );
    fn on_assignment_order_event(
        &mut self,
        contract: OptionContract,
        quantity: Decimal,
        is_assignment: bool,
        services: &mut dyn AlgorithmServices,
    );
    fn on_end_of_day(&mut self, symbol: Option<Symbol>, services: &mut dyn AlgorithmServices);
    fn on_warmup_finished(&mut self, services: &mut dyn AlgorithmServices);
    fn on_end_of_algorithm(&mut self, services: &mut dyn AlgorithmServices);
    fn on_margin_call(&mut self, requests: &[Order], services: &mut dyn AlgorithmServices);
    fn on_margin_call_warning(&mut self, services: &mut dyn AlgorithmServices);
    fn on_securities_changed(
        &mut self,
        changes: &SecurityChanges,
        services: &mut dyn AlgorithmServices,
    );
    fn on_splits(&mut self, splits: &HashMap<u64, Split>, services: &mut dyn AlgorithmServices);
    fn on_dividends(
        &mut self,
        dividends: &HashMap<u64, Dividend>,
        services: &mut dyn AlgorithmServices,
    );
    fn on_delistings(
        &mut self,
        delistings: &HashMap<u64, Delisting>,
        services: &mut dyn AlgorithmServices,
    );
    fn on_symbol_changed_events(
        &mut self,
        events: &HashMap<u64, SymbolChangedEvent>,
        services: &mut dyn AlgorithmServices,
    );
    fn select_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection>;
    fn select_custom_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        custom_data: &HashMap<String, Vec<CustomDataPoint>>,
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection>;
    fn select_fundamental_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        fundamentals: &[FundamentalData],
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        let _ = (utc_ns, resolution, fundamentals, services);
        Vec::new()
    }
    fn on_end_of_time_step(&mut self, services: &mut dyn AlgorithmServices);
    fn on_brokerage_message(&mut self, message: &str, services: &mut dyn AlgorithmServices);
    fn on_brokerage_disconnect(&mut self, services: &mut dyn AlgorithmServices);
    fn on_brokerage_reconnect(&mut self, services: &mut dyn AlgorithmServices);
    fn terminal_status(&self) -> Option<AlgorithmStatus> {
        None
    }
    fn runtime_error(&self) -> Option<String> {
        None
    }
    fn name(&self) -> &str;
    fn start_date(&self) -> DateTime;
    fn end_date(&self) -> DateTime;
    fn portfolio_value(&self) -> Price;
    fn starting_cash(&self) -> Price;
    /// Active market/custom subscription configs as shared handles. Returning
    /// `Arc`s (rather than deep-cloned owned values) lets the subscription-sync
    /// loop diff by the memoized `unique_id()` on the shared instances without
    /// re-hashing or reallocating every slice (issue #64).
    fn subscriptions(&self) -> Vec<Arc<SubscriptionDataConfig>>;
    /// Monotonic version stamp that changes whenever `subscriptions()` or
    /// `option_subscriptions()` would return a different set. The sync loop
    /// caches the last value and skips its diff entirely when unchanged.
    fn subscriptions_version(&self) -> u64;
    fn prepare_data_delivery(
        &mut self,
        subscriptions: &[SubscriptionDataConfig],
    ) -> anyhow::Result<()>;
    fn option_subscriptions(&self) -> Vec<OptionSubscription>;
    fn portfolio(&self) -> Option<Arc<crate::portfolio::SecurityPortfolioManager>>;
    fn transactions(&self) -> Option<Arc<TransactionManager>>;
    fn order_fee(&self, order: &Order, fill_price: Price) -> Price;
    fn contract_multiplier_for_symbol(&self, symbol: &Symbol) -> Price;
    fn validate_order_buying_power(
        &self,
        order: &Order,
        fill_price: Price,
        fee: Price,
    ) -> std::result::Result<(), String>;
    fn has_universes(&self) -> bool;
    fn universe_resolution(&self) -> Option<Resolution>;
    fn alpha_analytics(&self) -> AlphaAnalytics {
        AlphaAnalytics::default()
    }
    fn charts(&self) -> ChartCollection {
        ChartCollection::default()
    }
}

impl<T> LifecycleBridge for Box<T>
where
    T: LifecycleBridge + ?Sized,
{
    fn initialize(&mut self, services: &mut dyn AlgorithmServices) -> anyhow::Result<()> {
        self.as_mut().initialize(services)
    }

    fn on_data(&mut self, payload: DataDeliveryPayload, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_data(payload, services);
    }

    fn on_order_event(&mut self, event: &OrderEvent, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_order_event(event, services);
    }

    fn on_otm_expiry(
        &mut self,
        contract: OptionContract,
        quantity: Decimal,
        underlying_price: Decimal,
        entry_premium: Decimal,
        services: &mut dyn AlgorithmServices,
    ) {
        self.as_mut().on_otm_expiry(
            contract,
            quantity,
            underlying_price,
            entry_premium,
            services,
        );
    }

    fn on_assignment_order_event(
        &mut self,
        contract: OptionContract,
        quantity: Decimal,
        is_assignment: bool,
        services: &mut dyn AlgorithmServices,
    ) {
        self.as_mut()
            .on_assignment_order_event(contract, quantity, is_assignment, services);
    }

    fn on_end_of_day(&mut self, symbol: Option<Symbol>, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_end_of_day(symbol, services);
    }

    fn on_warmup_finished(&mut self, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_warmup_finished(services);
    }

    fn on_end_of_algorithm(&mut self, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_end_of_algorithm(services);
    }

    fn on_margin_call(&mut self, requests: &[Order], services: &mut dyn AlgorithmServices) {
        self.as_mut().on_margin_call(requests, services);
    }

    fn on_margin_call_warning(&mut self, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_margin_call_warning(services);
    }

    fn on_securities_changed(
        &mut self,
        changes: &SecurityChanges,
        services: &mut dyn AlgorithmServices,
    ) {
        self.as_mut().on_securities_changed(changes, services);
    }

    fn on_splits(&mut self, splits: &HashMap<u64, Split>, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_splits(splits, services);
    }

    fn on_dividends(
        &mut self,
        dividends: &HashMap<u64, Dividend>,
        services: &mut dyn AlgorithmServices,
    ) {
        self.as_mut().on_dividends(dividends, services);
    }

    fn on_delistings(
        &mut self,
        delistings: &HashMap<u64, Delisting>,
        services: &mut dyn AlgorithmServices,
    ) {
        self.as_mut().on_delistings(delistings, services);
    }

    fn on_symbol_changed_events(
        &mut self,
        events: &HashMap<u64, SymbolChangedEvent>,
        services: &mut dyn AlgorithmServices,
    ) {
        self.as_mut().on_symbol_changed_events(events, services);
    }

    fn select_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        self.as_mut()
            .select_universe_changes(utc_ns, resolution, services)
    }

    fn select_custom_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        custom_data: &HashMap<String, Vec<CustomDataPoint>>,
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        self.as_mut()
            .select_custom_universe_changes(utc_ns, resolution, custom_data, services)
    }

    fn select_fundamental_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        fundamentals: &[FundamentalData],
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        self.as_mut().select_fundamental_universe_changes(
            utc_ns,
            resolution,
            fundamentals,
            services,
        )
    }

    fn on_end_of_time_step(&mut self, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_end_of_time_step(services);
    }

    fn on_brokerage_message(&mut self, message: &str, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_brokerage_message(message, services);
    }

    fn on_brokerage_disconnect(&mut self, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_brokerage_disconnect(services);
    }

    fn on_brokerage_reconnect(&mut self, services: &mut dyn AlgorithmServices) {
        self.as_mut().on_brokerage_reconnect(services);
    }

    fn terminal_status(&self) -> Option<AlgorithmStatus> {
        self.as_ref().terminal_status()
    }

    fn runtime_error(&self) -> Option<String> {
        self.as_ref().runtime_error()
    }

    fn name(&self) -> &str {
        self.as_ref().name()
    }

    fn start_date(&self) -> DateTime {
        self.as_ref().start_date()
    }

    fn end_date(&self) -> DateTime {
        self.as_ref().end_date()
    }

    fn portfolio_value(&self) -> Price {
        self.as_ref().portfolio_value()
    }

    fn starting_cash(&self) -> Price {
        self.as_ref().starting_cash()
    }

    fn subscriptions(&self) -> Vec<Arc<SubscriptionDataConfig>> {
        self.as_ref().subscriptions()
    }

    fn subscriptions_version(&self) -> u64 {
        self.as_ref().subscriptions_version()
    }

    fn prepare_data_delivery(
        &mut self,
        subscriptions: &[SubscriptionDataConfig],
    ) -> anyhow::Result<()> {
        self.as_mut().prepare_data_delivery(subscriptions)
    }

    fn option_subscriptions(&self) -> Vec<OptionSubscription> {
        self.as_ref().option_subscriptions()
    }

    fn portfolio(&self) -> Option<Arc<crate::portfolio::SecurityPortfolioManager>> {
        self.as_ref().portfolio()
    }

    fn transactions(&self) -> Option<Arc<TransactionManager>> {
        self.as_ref().transactions()
    }

    fn order_fee(&self, order: &Order, fill_price: Price) -> Price {
        self.as_ref().order_fee(order, fill_price)
    }

    fn contract_multiplier_for_symbol(&self, symbol: &Symbol) -> Price {
        self.as_ref().contract_multiplier_for_symbol(symbol)
    }

    fn validate_order_buying_power(
        &self,
        order: &Order,
        fill_price: Price,
        fee: Price,
    ) -> std::result::Result<(), String> {
        self.as_ref()
            .validate_order_buying_power(order, fill_price, fee)
    }

    fn has_universes(&self) -> bool {
        self.as_ref().has_universes()
    }

    fn universe_resolution(&self) -> Option<Resolution> {
        self.as_ref().universe_resolution()
    }

    fn alpha_analytics(&self) -> AlphaAnalytics {
        self.as_ref().alpha_analytics()
    }

    fn charts(&self) -> ChartCollection {
        self.as_ref().charts()
    }
}

impl<T> AlgorithmStateAccess for Box<T>
where
    T: AlgorithmStateAccess + ?Sized,
{
    fn algorithm_state(&self) -> Option<Arc<Mutex<QcAlgorithm>>> {
        self.as_ref().algorithm_state()
    }
}

pub use LifecycleBridge as AlgorithmBridge;
