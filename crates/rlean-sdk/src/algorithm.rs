pub use rlean_algorithm::lifecycle::{
    AlgorithmBridge, AlgorithmRuntimeServices, AlgorithmStateAccess, OptionSubscription,
    RegisteredIndicatorBridge, RegisteredIndicatorRegistry, UniverseSelection,
};
use rlean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
use rlean_core::{
    DataNormalizationMode, DateTime, Market, Price, Resolution, SecurityType, Symbol,
    SymbolOptionsExt, TickType, TimeSpan,
};
use rlean_data::{CustomDataQuery, SubscriptionDataConfig, SubscriptionDataKind};
use rlean_orders::order::TimeInForce;
use rlean_orders::OrderTicket;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[cfg(feature = "python")]
use pyo3::types::{PyAnyMethods, PyTupleMethods};

use crate::data::ns_to_exchange_naive;
use crate::indicators::{
    AverageTrueRange, BollingerBandsIndicator, ExponentialMovingAverage, IdentityIndicator,
    MacdIndicator, MomentumPercentIndicator, RelativeStrengthIndex, SimpleMovingAverage,
    StandardDeviationIndicator,
};
use crate::orders::{OrderTicketContext, OrderTicketHandle};
use crate::portfolio::PortfolioView;
use crate::scheduling::ScheduleManagerHandle;
use crate::securities::{
    read_algorithm_security_price, set_algorithm_security_price_from_float,
    AlgorithmSettingsHandle, OptionSecurityHandle, SecurityHandle, SecurityManagerHandle,
    SymbolHandle,
};
use crate::types::MovingAverageType;
use crate::universe::{DateRulesHandle, TimeRulesHandle, UniverseSettings, UniverseSettingsHandle};

fn f2d(value: f64) -> Decimal {
    Decimal::from_f64(value).unwrap_or_default()
}

fn decimal_to_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

thread_local! {
    static DEFAULT_ALGORITHM_CONTEXT: RefCell<Option<AlgorithmConstructionContext>> = const { RefCell::new(None) };
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "QCAlgorithm", subclass))]
#[derive(Clone)]
pub struct AlgorithmHandle {
    inner: Arc<Mutex<QcAlgorithm>>,
    universe_settings: Arc<Mutex<UniverseSettings>>,
    algorithm_settings: Arc<Mutex<crate::securities::AlgorithmSettings>>,
    runtime_services: Arc<dyn AlgorithmRuntimeServices>,
    /// User-supplied security initializer (LEAN `ISecurityInitializer`). When set
    /// with a price seeder, the seeder runs on every security add.
    security_initializer: Arc<Mutex<Option<BrokerageModelSecurityInitializerHandle>>>,
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "BrokerageModel"))]
pub struct BrokerageModelHandle;

/// Native seed function: given a security handle bound to the algorithm, return
/// its seed price (mirrors C# LEAN's `Func<Security, BaseData>` seed function,
/// reduced to the last-known close price that rlean seeds).
pub type SecuritySeedFn = Arc<dyn Fn(&SecurityHandle) -> Option<f64> + Send + Sync>;

/// Seed a security's price from a seed function, mirroring C# LEAN's
/// `FuncSecuritySeeder` (`Lean/Common/Securities/FuncSecuritySeeder.cs`).
///
/// Universal: native Rust strategies construct it with a Rust seed function; the
/// pyo3 constructor stores the Python seed callable and adapts it to the same
/// native seed function.
#[derive(Clone, Default)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "FuncSecuritySeeder"))]
pub struct FuncSecuritySeederHandle {
    seed_fn: Option<SecuritySeedFn>,
}

impl FuncSecuritySeederHandle {
    /// Construct a seeder from a native Rust seed function.
    pub fn from_fn(seed_fn: SecuritySeedFn) -> Self {
        Self {
            seed_fn: Some(seed_fn),
        }
    }

    /// Seed the security, mirroring `FuncSecuritySeeder.SeedSecurity`: skip
    /// canonical symbols, otherwise apply the seed price. Returns the seeded
    /// price when one was produced.
    pub fn seed_price(&self, security: &SecurityHandle) -> Option<f64> {
        if security.symbol_inner().is_canonical_option() {
            return None;
        }
        let seed_fn = self.seed_fn.as_ref()?;
        seed_fn(security).filter(|price| *price > 0.0 && price.is_finite())
    }
}

/// Security initializer, mirroring C# LEAN's `BrokerageModelSecurityInitializer`
/// (`Lean/Common/Securities/BrokerageModelSecurityInitializer.cs`). rlean already
/// sets the brokerage models on every security add, so the SDK-facing initializer
/// only carries the optional price seeder that runs on add.
#[derive(Clone, Default)]
#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "BrokerageModelSecurityInitializer")
)]
pub struct BrokerageModelSecurityInitializerHandle {
    seeder: Option<FuncSecuritySeederHandle>,
}

impl BrokerageModelSecurityInitializerHandle {
    /// Construct an initializer carrying the given price seeder.
    pub fn with_seeder(seeder: FuncSecuritySeederHandle) -> Self {
        Self {
            seeder: Some(seeder),
        }
    }

    /// The configured price seeder, if any.
    pub fn seeder(&self) -> Option<&FuncSecuritySeederHandle> {
        self.seeder.as_ref()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl BrokerageModelHandle {
    #[new]
    fn py_new() -> Self {
        Self
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl FuncSecuritySeederHandle {
    /// `FuncSecuritySeeder(seed_function)` — stores the Python seed callable. The
    /// callable is invoked with the `Security` being seeded and must return its
    /// seed price (e.g. `self.get_last_known_prices`).
    #[new]
    #[pyo3(signature = (seed_function=None))]
    fn py_new(seed_function: Option<pyo3::Py<pyo3::PyAny>>) -> Self {
        let Some(seed_function) = seed_function else {
            return Self::default();
        };
        let seed_fn: SecuritySeedFn = Arc::new(move |security: &SecurityHandle| {
            pyo3::Python::attach(|py| {
                let result = seed_function.call1(py, (security.clone(),)).ok()?;
                py_seed_result_to_price(py, result)
            })
        });
        Self::from_fn(seed_fn)
    }
}

/// Coerce a Python seed-function result into a price. Accepts a plain number
/// (rlean's `get_last_known_prices` returns a float) or a `Security` handle whose
/// current price is read back.
#[cfg(feature = "python")]
fn py_seed_result_to_price(py: pyo3::Python<'_>, result: pyo3::Py<pyo3::PyAny>) -> Option<f64> {
    use pyo3::types::PyAnyMethods;
    let bound = result.bind(py);
    if bound.is_none() {
        return None;
    }
    if let Ok(price) = bound.extract::<f64>() {
        return Some(price);
    }
    if let Ok(security) = bound.extract::<SecurityHandle>() {
        let price = security.price();
        if price > 0.0 {
            return Some(price);
        }
    }
    None
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl BrokerageModelSecurityInitializerHandle {
    /// `BrokerageModelSecurityInitializer(brokerage_model, seeder=None)` — the
    /// brokerage model is applied by rlean on every security add already, so only
    /// the optional seeder is retained.
    #[new]
    #[pyo3(signature = (_brokerage_model=None, seeder=None))]
    fn py_new(
        _brokerage_model: Option<pyo3::Py<pyo3::PyAny>>,
        seeder: Option<FuncSecuritySeederHandle>,
    ) -> Self {
        match seeder {
            Some(seeder) => Self::with_seeder(seeder),
            None => Self::default(),
        }
    }
}

#[derive(Clone)]
pub struct AlgorithmConstructionContext {
    state: Arc<Mutex<QcAlgorithm>>,
    runtime_services: Arc<dyn AlgorithmRuntimeServices>,
    algorithm_settings: Arc<Mutex<crate::securities::AlgorithmSettings>>,
}

impl AlgorithmConstructionContext {
    pub fn new_with_runtime_services(
        state: Arc<Mutex<QcAlgorithm>>,
        runtime_services: Arc<dyn AlgorithmRuntimeServices>,
    ) -> Self {
        Self {
            state,
            runtime_services,
            algorithm_settings: Arc::new(Mutex::new(
                crate::securities::AlgorithmSettings::default(),
            )),
        }
    }

    /// Shared algorithm-settings store carried into constructed algorithm handles.
    pub fn algorithm_settings(&self) -> Arc<Mutex<crate::securities::AlgorithmSettings>> {
        self.algorithm_settings.clone()
    }

    pub fn registered_indicators(&self) -> RegisteredIndicatorRegistry {
        self.runtime_services.registered_indicators()
    }

    pub fn runtime_services(&self) -> Arc<dyn AlgorithmRuntimeServices> {
        self.runtime_services.clone()
    }

    pub fn state(&self) -> Arc<Mutex<QcAlgorithm>> {
        self.state.clone()
    }
}

impl AlgorithmHandle {
    pub fn default_algorithm() -> Self {
        if let Some(context) = DEFAULT_ALGORITHM_CONTEXT.with(|state| state.borrow().clone()) {
            return Self {
                inner: context.state,
                universe_settings: Arc::new(Mutex::new(UniverseSettings::default())),
                algorithm_settings: context.algorithm_settings,
                runtime_services: context.runtime_services,
                security_initializer: Arc::new(Mutex::new(None)),
            };
        }

        panic!("QCAlgorithm must be constructed inside an AlgorithmConstructionContext")
    }

    pub fn inner(&self) -> Arc<Mutex<QcAlgorithm>> {
        self.inner.clone()
    }

    pub fn runtime_services(&self) -> Arc<dyn AlgorithmRuntimeServices> {
        self.runtime_services.clone()
    }

    fn register_indicator_handle(
        &self,
        symbol: &SymbolHandle,
        indicator: Arc<dyn RegisteredIndicatorBridge>,
    ) {
        self.runtime_services
            .registered_indicators()
            .lock()
            .expect("registered indicator registry poisoned")
            .entry(symbol.sid())
            .or_default()
            .push(indicator);
    }

    pub fn with_default_context<T>(
        context: AlgorithmConstructionContext,
        f: impl FnOnce() -> T,
    ) -> T {
        DEFAULT_ALGORITHM_CONTEXT.with(|default_state| {
            let previous = default_state.replace(Some(context));
            let result = f();
            default_state.replace(previous);
            result
        })
    }
    pub fn portfolio(&self) -> PortfolioView {
        PortfolioView::from_algorithm(self.inner.clone())
    }
    /// Security manager bound to this algorithm for symbol lookup.
    pub fn securities(&self) -> SecurityManagerHandle {
        SecurityManagerHandle::new(self.inner())
    }
    pub fn universe_settings(&self) -> UniverseSettingsHandle {
        UniverseSettingsHandle::from_shared(self.universe_settings.clone())
    }

    pub fn algorithm_settings(&self) -> AlgorithmSettingsHandle {
        AlgorithmSettingsHandle::from_shared(self.algorithm_settings.clone(), self.inner.clone())
    }
    pub fn date_rules(&self) -> DateRulesHandle {
        DateRulesHandle::new()
    }
    pub fn time_rules(&self) -> TimeRulesHandle {
        TimeRulesHandle::new()
    }

    pub fn schedule(&self) -> ScheduleManagerHandle {
        ScheduleManagerHandle::new(self.runtime_services.clone())
    }

    fn order_ticket_context(&self) -> OrderTicketContext {
        let algorithm = self.inner.clone();
        let transactions = self.inner.lock().unwrap().transactions.clone();
        OrderTicketContext::new(transactions, move || algorithm.lock().unwrap().utc_time)
    }

    pub fn set_start_date(&self, year: i32, month: u32, day: u32) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_start_date(year, month, day);
    }

    pub fn set_end_date(&self, year: i32, month: u32, day: u32) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_end_date(year, month, day);
    }

    pub fn set_cash(&self, amount: f64) {
        let mut algorithm = self.inner.lock().unwrap();
        AlgorithmApi::new(&mut algorithm).set_cash(amount);
        let cash = algorithm.cash();
        *algorithm.portfolio.cash.write() = cash;
    }

    pub fn add_cash(&self, amount: f64) {
        let mut algorithm = self.inner.lock().unwrap();
        AlgorithmApi::new(&mut algorithm).add_cash(amount);
        let cash = algorithm.cash();
        *algorithm.portfolio.cash.write() = cash;
    }
    pub fn cash(&self) -> f64 {
        decimal_to_f64(AlgorithmApi::new(&mut self.inner.lock().unwrap()).cash())
    }
    pub fn portfolio_value(&self) -> f64 {
        decimal_to_f64(AlgorithmApi::new(&mut self.inner.lock().unwrap()).portfolio_value())
    }

    pub fn set_name(&self, name: String) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_name(&name);
    }

    pub fn set_brokerage_model(&self, brokerage: BrokerageName, account_type: AccountType) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .set_brokerage_model(brokerage, account_type);
    }

    pub fn brokerage_model(&self) -> BrokerageModelHandle {
        BrokerageModelHandle
    }

    /// Store the security initializer, mirroring C# LEAN's
    /// `QCAlgorithm.SetSecurityInitializer`. Its price seeder (if any) runs when a
    /// security is added (`add_equity` / `add_option` / `add_security`).
    pub fn set_security_initializer(&self, initializer: BrokerageModelSecurityInitializerHandle) {
        *self
            .security_initializer
            .lock()
            .expect("security initializer poisoned") = Some(initializer);
    }

    /// Rust equivalent of C# LEAN's `QCAlgorithm.GetLastKnownPrices(Symbol)`: a real
    /// history-provider lookup for seeding a security's price, not a re-read of the
    /// security's (possibly still-zero) live price.
    pub fn get_last_known_prices(&self, security: SecurityHandle) -> Option<f64> {
        let symbol = security.symbol_inner();
        let resolution = {
            let algorithm = self.inner.lock().unwrap();
            algorithm
                .subscription_manager
                .get_configs_for_symbol(symbol)
                .iter()
                .map(|config| config.resolution)
                .min()
                .unwrap_or(Resolution::Minute)
        };
        let algorithm = self.inner.lock().unwrap();
        self.runtime_services
            .history_service()
            .last_known_close_price(&algorithm, symbol, resolution)
    }

    pub fn set_benchmark(&self, ticker: String) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_benchmark(&ticker);
    }

    pub fn get_parameter(&self, key: String, default: Option<String>) -> Option<String> {
        self.runtime_services
            .runtime_parameters()
            .read()
            .expect("runtime parameters poisoned")
            .get(&key)
            .cloned()
            .or(default)
    }

    pub fn set_parameter(&self, key: String, value: String) {
        self.runtime_services
            .runtime_parameters()
            .write()
            .expect("runtime parameters poisoned")
            .insert(key, value);
    }

    pub fn has_security(&self, symbol: SymbolHandle) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).has_security(symbol.inner())
    }

    pub fn is_invested(&self, symbol: SymbolHandle) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).is_invested(symbol.inner())
    }
    pub fn current_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .current_time()
                .0,
        )
    }
    pub fn utc_time(&self) -> chrono::NaiveDateTime {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .utc_time()
            .to_utc()
            .naive_utc()
    }
    pub fn is_warming_up(&self) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).is_warming_up()
    }
    pub fn live_mode(&self) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).live_mode()
    }

    pub fn history(
        &self,
        symbol: Symbol,
        periods: usize,
        resolution: Resolution,
    ) -> HashMap<String, Vec<String>> {
        let algorithm = self.inner.lock().unwrap();
        self.runtime_services
            .history_service()
            .history(&algorithm, &symbol, periods, resolution)
    }

    pub fn set_warm_up_int(&self, n: i64, resolution: Option<Resolution>) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_warm_up_int(n, resolution);
    }

    pub fn set_warm_up(&self, n: i64, resolution: Option<Resolution>) {
        self.set_warm_up_int(n, resolution);
    }

    pub fn add_equity(
        &self,
        ticker: String,
        resolution: Resolution,
        leverage: Option<f64>,
    ) -> SecurityHandle {
        let symbol = {
            let mut algorithm = self.inner.lock().unwrap();
            let symbol = AlgorithmApi::new(&mut algorithm)
                .add_equity_with_normalization(&ticker, resolution, None);
            if let Some(leverage) = leverage {
                algorithm.register_security_leverage(&symbol, leverage);
            }
            symbol
        };
        self.apply_security_initializer(&symbol, resolution);
        SecurityHandle::with_algorithm(symbol, self.inner.clone())
    }

    /// Seed a newly added security's price, mirroring C# LEAN's
    /// `SecurityInitializer.Initialize(security)` seeding step. When a user
    /// initializer with a price seeder is set, invoke it; otherwise fall back to
    /// rlean's default history seeding. Skips securities that already have a
    /// non-zero price.
    fn apply_security_initializer(&self, symbol: &Symbol, resolution: Resolution) {
        // C# FuncSecuritySeeder never attempts history for canonical
        // derivatives. They are universe handles, not tradable securities, and
        // have no last-known market price of their own.
        if symbol.is_canonical_option() {
            return;
        }
        if read_algorithm_security_price(&self.inner, symbol)
            .map(|price| price > 0.0)
            .unwrap_or(false)
        {
            return;
        }

        let seeder = self
            .security_initializer
            .lock()
            .expect("security initializer poisoned")
            .as_ref()
            .and_then(|initializer| initializer.seeder().cloned());

        if let Some(seeder) = seeder {
            let security = SecurityHandle::with_algorithm(symbol.clone(), self.inner.clone());
            if let Some(price) = seeder.seed_price(&security) {
                let _ = set_algorithm_security_price_from_float(&self.inner, symbol, price);
            }
            return;
        }

        self.seed_security_price_from_history(symbol, resolution);
    }

    fn seed_security_price_from_history(&self, symbol: &Symbol, resolution: Resolution) {
        let price = {
            let algorithm = self.inner.lock().unwrap();
            self.runtime_services
                .history_service()
                .last_known_close_price(&algorithm, symbol, resolution)
        };
        if let Some(price) = price {
            let _ = set_algorithm_security_price_from_float(&self.inner, symbol, price);
        }
    }

    pub fn add_forex(&self, ticker: String, resolution: Resolution) -> SymbolHandle {
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap()).add_forex(&ticker, resolution),
        )
    }

    pub fn add_crypto(
        &self,
        ticker: String,
        market: Option<String>,
        resolution: Resolution,
    ) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .add_crypto(&ticker, &market, resolution),
        )
    }

    pub fn add_crypto_future(
        &self,
        ticker: String,
        market: Option<String>,
        resolution: Resolution,
        leverage: Option<f64>,
    ) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .add_crypto_future(&ticker, &market, resolution, leverage),
        )
    }

    pub fn add_security(
        &self,
        security_type: SecurityType,
        ticker: String,
        resolution: Resolution,
    ) -> SymbolHandle {
        let market = Market::usa();
        let symbol = Symbol::create_with_security_type(&ticker, security_type, Some(market));
        let symbol = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .add_security_symbol(symbol, resolution);
        self.apply_security_initializer(&symbol, resolution);
        SymbolHandle::new(symbol)
    }

    pub fn add_option(
        &self,
        underlying_ticker: String,
        resolution: Resolution,
    ) -> OptionSecurityHandle {
        let symbol = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .add_option(&underlying_ticker, resolution);
        // The canonical option symbol is skipped by the seeder (LEAN
        // `FuncSecuritySeeder` skips canonical symbols); calling the initializer
        // here keeps the add path uniform and seeds any non-canonical add path.
        self.apply_security_initializer(&symbol, resolution);
        OptionSecurityHandle::new(symbol, self.inner.clone())
    }

    pub fn add_option_contract(
        &self,
        symbol: SymbolHandle,
        resolution: Resolution,
    ) -> SymbolHandle {
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .add_option_contract(symbol.into_inner(), resolution),
        )
    }

    pub fn add_option_quote_contract(
        &self,
        symbol: SymbolHandle,
        resolution: Resolution,
    ) -> SymbolHandle {
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .add_option_quote_contract(symbol.into_inner(), resolution),
        )
    }

    pub fn add_data(
        &self,
        source_type: String,
        ticker: String,
        resolution: Resolution,
        properties: Option<HashMap<String, String>>,
    ) -> SecurityHandle {
        SecurityHandle::with_algorithm(
            self.inner.lock().unwrap().add_custom_data(
                &source_type,
                &ticker,
                resolution,
                properties.unwrap_or_default(),
            ),
            self.inner.clone(),
        )
    }

    pub fn add_data_with_properties(
        &self,
        source_type: String,
        ticker: String,
        resolution: Resolution,
        properties: HashMap<String, String>,
    ) -> SymbolHandle {
        SymbolHandle::new(self.inner.lock().unwrap().add_custom_data(
            &source_type,
            &ticker,
            resolution,
            properties,
        ))
    }

    pub fn log(&self, message: String) {
        self.inner.lock().unwrap().log_message(message);
    }

    pub fn debug(&self, message: String) {
        self.inner.lock().unwrap().debug(message);
    }

    pub fn error(&self, message: String) {
        self.inner.lock().unwrap().error(message);
    }

    pub fn remove_security(&self, symbol: SymbolHandle, tag: Option<String>) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .remove_security(symbol.inner(), tag.as_deref())
    }

    pub fn remove_option_contract(&self, symbol: SymbolHandle, tag: Option<String>) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .remove_option_contract(symbol.inner(), tag.as_deref())
    }

    pub fn market_order(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: Option<bool>,
    ) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap()).market_order(
            symbol.inner(),
            quantity,
            time_in_force,
            outside_regular_trading_hours.unwrap_or(false),
        );
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    pub fn buy(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        self.market_order(symbol, quantity.abs(), None, None)
    }

    pub fn sell(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        self.market_order(symbol, -quantity.abs(), None, None)
    }

    pub fn calculate_order_quantity(&self, symbol: SymbolHandle, target: f64) -> f64 {
        let algorithm = self.inner.lock().unwrap();
        let Some(security) = algorithm.securities.get(symbol.inner()) else {
            return 0.0;
        };
        let current_price = security.current_price();
        if current_price.is_zero() {
            return 0.0;
        }
        let portfolio_value = algorithm.portfolio.total_portfolio_value();
        let current_holding = algorithm.portfolio.get_holding(symbol.inner());
        let target_value = portfolio_value * f2d(target);
        let target_quantity = target_value / current_price;
        decimal_to_f64((target_quantity - current_holding.quantity).trunc())
    }

    pub fn limit_order(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        limit_price: f64,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
        post_only: bool,
    ) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap()).limit_order(
            symbol.inner(),
            quantity,
            limit_price,
            time_in_force,
            outside_regular_trading_hours,
            post_only,
        );
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    pub fn stop_market_order(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        stop_price: f64,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap()).stop_market_order(
            symbol.inner(),
            quantity,
            stop_price,
            time_in_force,
            outside_regular_trading_hours,
        );
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    pub fn market_on_open_order(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .market_on_open_order(symbol.inner(), quantity);
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    pub fn market_on_close_order(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .market_on_close_order(symbol.inner(), quantity);
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    pub fn set_holdings(&self, symbol: SymbolHandle, target: f64) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_holdings(symbol.inner(), target);
    }

    pub fn liquidate(&self, symbol: Option<SymbolHandle>) {
        let symbol = symbol.as_ref().map(SymbolHandle::inner);
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).liquidate(symbol);
    }

    pub fn sell_to_open(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        premium_per_contract: f64,
    ) -> i64 {
        self.inner.lock().unwrap().sell_to_open(
            symbol.into_inner(),
            f2d(quantity),
            f2d(premium_per_contract),
        )
    }

    pub fn buy_to_open(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        premium_per_contract: f64,
    ) -> i64 {
        self.inner.lock().unwrap().buy_to_open(
            symbol.into_inner(),
            f2d(quantity),
            f2d(premium_per_contract),
        )
    }

    pub fn buy_to_close(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        premium_per_contract: f64,
    ) -> i64 {
        self.inner.lock().unwrap().buy_to_close(
            symbol.into_inner(),
            f2d(quantity),
            f2d(premium_per_contract),
        )
    }

    pub fn sell_to_close(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        premium_per_contract: f64,
    ) -> i64 {
        self.inner.lock().unwrap().sell_to_close(
            symbol.into_inner(),
            f2d(quantity),
            f2d(premium_per_contract),
        )
    }

    pub fn sma(
        &self,
        symbol: SymbolHandle,
        period: usize,
        _resolution: Option<Resolution>,
    ) -> SimpleMovingAverage {
        let indicator = SimpleMovingAverage::new(period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn ema(
        &self,
        symbol: SymbolHandle,
        period: usize,
        _resolution: Option<Resolution>,
    ) -> ExponentialMovingAverage {
        let indicator = ExponentialMovingAverage::new(period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn rsi(
        &self,
        symbol: SymbolHandle,
        period: usize,
        _moving_average_type: Option<MovingAverageType>,
        _resolution: Option<Resolution>,
    ) -> RelativeStrengthIndex {
        let indicator = RelativeStrengthIndex::new(period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn momp(
        &self,
        symbol: SymbolHandle,
        period: usize,
        _resolution: Option<Resolution>,
    ) -> MomentumPercentIndicator {
        let indicator = MomentumPercentIndicator::new(period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn std(
        &self,
        symbol: SymbolHandle,
        period: usize,
        _resolution: Option<Resolution>,
    ) -> StandardDeviationIndicator {
        let indicator = StandardDeviationIndicator::new(period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn bb(
        &self,
        symbol: SymbolHandle,
        period: usize,
        k: Option<f64>,
        _moving_average_type: Option<MovingAverageType>,
        _resolution: Option<Resolution>,
    ) -> BollingerBandsIndicator {
        let indicator = BollingerBandsIndicator::new(period, k.unwrap_or(2.0));
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn macd(
        &self,
        symbol: SymbolHandle,
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        _moving_average_type: Option<MovingAverageType>,
        _resolution: Option<Resolution>,
    ) -> MacdIndicator {
        let indicator = MacdIndicator::new(fast_period, slow_period, signal_period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn atr(
        &self,
        symbol: SymbolHandle,
        period: usize,
        _moving_average_type: Option<MovingAverageType>,
        _resolution: Option<Resolution>,
    ) -> AverageTrueRange {
        let indicator = AverageTrueRange::new(period);
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn identity(
        &self,
        symbol: SymbolHandle,
        _resolution: Option<Resolution>,
    ) -> IdentityIndicator {
        let indicator = IdentityIndicator::new();
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    pub fn register_indicator(
        &self,
        _symbol: SymbolHandle,
        _indicator: IdentityIndicator,
        _resolution: Option<Resolution>,
    ) {
    }

    pub fn warm_up_indicator(
        &self,
        _symbol: SymbolHandle,
        _indicator: IdentityIndicator,
        _resolution: Option<Resolution>,
    ) {
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl AlgorithmHandle {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn py_new(
        _args: &pyo3::Bound<'_, pyo3::types::PyTuple>,
        _kwargs: Option<&pyo3::Bound<'_, pyo3::types::PyDict>>,
    ) -> Self {
        Self::default_algorithm()
    }

    #[pyo3(name = "set_start_date")]
    fn py_set_start_date(&self, year: i32, month: u32, day: u32) {
        self.set_start_date(year, month, day);
    }

    #[pyo3(name = "set_end_date")]
    fn py_set_end_date(&self, year: i32, month: u32, day: u32) {
        self.set_end_date(year, month, day);
    }

    #[pyo3(name = "set_cash")]
    fn py_set_cash(&self, amount: f64) {
        self.set_cash(amount);
    }

    #[pyo3(name = "set_brokerage_model")]
    fn py_set_brokerage_model(
        &self,
        brokerage: crate::types::BrokerageName,
        account_type: crate::types::AccountType,
    ) {
        self.set_brokerage_model(brokerage.into(), account_type.into());
    }

    #[getter(brokerage_model)]
    fn py_brokerage_model(&self) -> BrokerageModelHandle {
        self.brokerage_model()
    }

    #[pyo3(name = "set_security_initializer")]
    fn py_set_security_initializer(&self, initializer: BrokerageModelSecurityInitializerHandle) {
        self.set_security_initializer(initializer);
    }

    #[pyo3(name = "get_last_known_prices")]
    fn py_get_last_known_prices(&self, security: SecurityHandle) -> Option<f64> {
        self.get_last_known_prices(security)
    }

    #[pyo3(name = "set_benchmark")]
    fn py_set_benchmark(&self, ticker: String) {
        self.set_benchmark(ticker);
    }

    #[pyo3(name = "add_cash")]
    fn py_add_cash(&self, amount: f64) {
        self.add_cash(amount);
    }

    #[getter(cash)]
    fn py_cash(&self) -> f64 {
        self.cash()
    }

    #[getter(portfolio_value)]
    fn py_portfolio_value(&self) -> f64 {
        self.portfolio_value()
    }

    #[getter(portfolio)]
    fn py_portfolio(&self) -> PortfolioView {
        self.portfolio()
    }

    #[getter(is_warming_up)]
    fn py_is_warming_up(&self) -> bool {
        self.is_warming_up()
    }

    #[getter(live_mode)]
    fn py_live_mode(&self) -> bool {
        self.live_mode()
    }

    #[pyo3(name = "get_parameter", signature = (key, default=None))]
    fn py_get_parameter(&self, key: String, default: Option<String>) -> Option<String> {
        self.get_parameter(key, default)
    }

    #[pyo3(name = "set_parameter")]
    fn py_set_parameter(&self, key: String, value: String) {
        self.set_parameter(key, value);
    }

    #[pyo3(name = "log")]
    fn py_log(&self, message: String) {
        self.log(message);
    }

    #[pyo3(name = "debug")]
    fn py_debug(&self, message: String) {
        self.debug(message);
    }

    #[pyo3(name = "error")]
    fn py_error(&self, message: String) {
        self.error(message);
    }

    #[pyo3(name = "has_security")]
    fn py_has_security(&self, symbol: SymbolHandle) -> bool {
        self.has_security(symbol)
    }

    #[pyo3(name = "history")]
    fn py_history(
        &self,
        symbol: SymbolHandle,
        periods: usize,
        resolution: crate::types::Resolution,
    ) -> HashMap<String, Vec<String>> {
        self.history(symbol.into_inner(), periods, resolution.into())
    }

    #[pyo3(name = "add_equity", signature = (ticker, resolution, leverage=None))]
    fn py_add_equity(
        &self,
        ticker: String,
        resolution: crate::types::Resolution,
        leverage: Option<f64>,
    ) -> SecurityHandle {
        self.add_equity(ticker, resolution.into(), leverage)
    }

    #[pyo3(name = "add_option")]
    fn py_add_option(
        &self,
        underlying_ticker: String,
        resolution: crate::types::Resolution,
    ) -> OptionSecurityHandle {
        self.add_option(underlying_ticker, resolution.into())
    }

    #[pyo3(name = "add_option_contract")]
    fn py_add_option_contract(
        &self,
        symbol: SymbolHandle,
        resolution: crate::types::Resolution,
    ) -> SymbolHandle {
        self.add_option_contract(symbol, resolution.into())
    }

    #[pyo3(name = "add_option_quote_contract")]
    fn py_add_option_quote_contract(
        &self,
        symbol: SymbolHandle,
        resolution: crate::types::Resolution,
    ) -> SymbolHandle {
        self.add_option_quote_contract(symbol, resolution.into())
    }

    #[pyo3(name = "add_data", signature = (source_type, ticker, resolution, properties=None))]
    fn py_add_data(
        &self,
        source_type: String,
        ticker: String,
        resolution: crate::types::Resolution,
        properties: Option<HashMap<String, String>>,
    ) -> SecurityHandle {
        self.add_data(source_type, ticker, resolution.into(), properties)
    }

    #[pyo3(name = "remove_security", signature = (symbol, tag=None))]
    fn py_remove_security(&self, symbol: SymbolHandle, tag: Option<String>) -> bool {
        self.remove_security(symbol, tag)
    }

    #[pyo3(name = "remove_option_contract", signature = (symbol, tag=None))]
    fn py_remove_option_contract(&self, symbol: SymbolHandle, tag: Option<String>) -> bool {
        self.remove_option_contract(symbol, tag)
    }

    #[pyo3(name = "set_warm_up", signature = (n, resolution=None))]
    fn py_set_warm_up(&self, n: i64, resolution: Option<crate::types::Resolution>) {
        self.set_warm_up(n, resolution.map(Into::into));
    }

    #[pyo3(name = "market_order", signature = (symbol, quantity, time_in_force=None, outside_regular_trading_hours=None))]
    fn py_market_order(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        time_in_force: Option<crate::types::TimeInForce>,
        outside_regular_trading_hours: Option<bool>,
    ) -> OrderTicketHandle {
        self.market_order(
            symbol,
            quantity,
            time_in_force.map(Into::into),
            outside_regular_trading_hours,
        )
    }

    #[pyo3(name = "limit_order", signature = (symbol, quantity, limit_price, time_in_force=None, outside_regular_trading_hours=false, post_only=false))]
    fn py_limit_order(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        limit_price: f64,
        time_in_force: Option<crate::types::TimeInForce>,
        outside_regular_trading_hours: bool,
        post_only: bool,
    ) -> OrderTicketHandle {
        self.limit_order(
            symbol,
            quantity,
            limit_price,
            time_in_force.map(Into::into),
            outside_regular_trading_hours,
            post_only,
        )
    }

    #[pyo3(name = "stop_market_order", signature = (symbol, quantity, stop_price, time_in_force=None, outside_regular_trading_hours=false))]
    fn py_stop_market_order(
        &self,
        symbol: SymbolHandle,
        quantity: f64,
        stop_price: f64,
        time_in_force: Option<crate::types::TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicketHandle {
        self.stop_market_order(
            symbol,
            quantity,
            stop_price,
            time_in_force.map(Into::into),
            outside_regular_trading_hours,
        )
    }

    #[pyo3(name = "set_holdings", signature = (symbol, target, liquidate_existing_holdings=false, tag=None))]
    fn py_set_holdings(
        &self,
        symbol: SymbolHandle,
        target: f64,
        liquidate_existing_holdings: bool,
        tag: Option<String>,
    ) {
        let _ = (liquidate_existing_holdings, tag);
        self.set_holdings(symbol, target);
    }

    #[pyo3(name = "liquidate", signature = (symbol=None, tag=None))]
    fn py_liquidate(&self, symbol: Option<SymbolHandle>, tag: Option<String>) {
        let _ = tag;
        self.liquidate(symbol);
    }

    #[pyo3(name = "sma")]
    fn py_sma(
        &self,
        symbol: SymbolHandle,
        period: usize,
        resolution: crate::types::Resolution,
    ) -> SimpleMovingAverage {
        self.sma(symbol, period, Some(resolution.into()))
    }

    #[pyo3(name = "ema")]
    fn py_ema(
        &self,
        symbol: SymbolHandle,
        period: usize,
        resolution: crate::types::Resolution,
    ) -> ExponentialMovingAverage {
        self.ema(symbol, period, Some(resolution.into()))
    }

    #[pyo3(name = "rsi")]
    fn py_rsi(
        &self,
        symbol: SymbolHandle,
        period: usize,
        moving_average_type: MovingAverageType,
        resolution: crate::types::Resolution,
    ) -> RelativeStrengthIndex {
        self.rsi(
            symbol,
            period,
            Some(moving_average_type),
            Some(resolution.into()),
        )
    }

    #[pyo3(name = "momp")]
    fn py_momp(
        &self,
        symbol: SymbolHandle,
        period: usize,
        resolution: crate::types::Resolution,
    ) -> MomentumPercentIndicator {
        self.momp(symbol, period, Some(resolution.into()))
    }

    #[pyo3(name = "std")]
    fn py_std(
        &self,
        symbol: SymbolHandle,
        period: usize,
        resolution: crate::types::Resolution,
    ) -> StandardDeviationIndicator {
        self.std(symbol, period, Some(resolution.into()))
    }

    #[pyo3(name = "bb")]
    fn py_bb(
        &self,
        symbol: SymbolHandle,
        period: usize,
        k: f64,
        moving_average_type: MovingAverageType,
        resolution: crate::types::Resolution,
    ) -> BollingerBandsIndicator {
        self.bb(
            symbol,
            period,
            Some(k),
            Some(moving_average_type),
            Some(resolution.into()),
        )
    }

    #[pyo3(name = "macd")]
    fn py_macd(
        &self,
        symbol: SymbolHandle,
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        moving_average_type: MovingAverageType,
        resolution: crate::types::Resolution,
    ) -> MacdIndicator {
        self.macd(
            symbol,
            fast_period,
            slow_period,
            signal_period,
            Some(moving_average_type),
            Some(resolution.into()),
        )
    }

    #[pyo3(name = "identity")]
    fn py_identity(
        &self,
        symbol: SymbolHandle,
        resolution: crate::types::Resolution,
    ) -> IdentityIndicator {
        self.identity(symbol, Some(resolution.into()))
    }

    #[pyo3(name = "register_indicator", signature = (symbol, indicator, resolution=None))]
    fn py_register_indicator(
        &self,
        symbol: SymbolHandle,
        indicator: IdentityIndicator,
        resolution: Option<crate::types::Resolution>,
    ) {
        self.register_indicator(symbol, indicator, resolution.map(Into::into));
    }

    #[pyo3(name = "warm_up_indicator", signature = (symbol, indicator, resolution=None))]
    fn py_warm_up_indicator(
        &self,
        symbol: SymbolHandle,
        indicator: IdentityIndicator,
        resolution: Option<crate::types::Resolution>,
    ) {
        self.warm_up_indicator(symbol, indicator, resolution.map(Into::into));
    }

    #[pyo3(name = "add_alpha")]
    fn py_add_alpha(
        &self,
        py: pyo3::Python<'_>,
        model: pyo3::Py<pyo3::PyAny>,
    ) -> pyo3::PyResult<()> {
        crate::python_framework::register_alpha(py, self, model)
    }

    #[pyo3(name = "set_portfolio_construction")]
    fn py_set_portfolio_construction(
        &self,
        py: pyo3::Python<'_>,
        model: pyo3::Py<pyo3::PyAny>,
    ) -> pyo3::PyResult<()> {
        crate::python_framework::register_portfolio_construction(py, self, model)
    }

    #[pyo3(name = "set_execution")]
    fn py_set_execution(
        &self,
        py: pyo3::Python<'_>,
        model: pyo3::Py<pyo3::PyAny>,
    ) -> pyo3::PyResult<()> {
        crate::python_framework::register_execution(py, self, model)
    }

    #[pyo3(name = "set_risk_management")]
    fn py_set_risk_management(
        &self,
        py: pyo3::Python<'_>,
        model: pyo3::Py<pyo3::PyAny>,
    ) -> pyo3::PyResult<()> {
        crate::python_framework::register_risk_management(py, self, model)
    }

    #[getter(insights)]
    fn py_insights(&self) -> pyo3::PyResult<crate::python_framework::InsightCollectionHandle> {
        let registry = crate::python_framework::framework_registry(self)?;
        Ok(crate::python_framework::InsightCollectionHandle::from_registry(&registry))
    }

    #[getter(securities)]
    fn py_securities(&self) -> SecurityManagerHandle {
        self.securities()
    }

    #[getter(time)]
    fn py_time(&self) -> chrono::NaiveDateTime {
        self.current_time()
    }

    #[getter(utc_time)]
    fn py_utc_time(&self) -> chrono::NaiveDateTime {
        self.utc_time()
    }

    #[getter(universe_settings)]
    fn py_universe_settings(&self) -> UniverseSettingsHandle {
        self.universe_settings()
    }

    #[pyo3(name = "add_universe", signature = (*args))]
    fn py_add_universe(
        &self,
        py: pyo3::Python<'_>,
        args: &pyo3::Bound<'_, pyo3::types::PyTuple>,
    ) -> pyo3::PyResult<()> {
        match args.len() {
            // Modern LEAN overload: AddUniverse(fundamental_selector).
            1 => crate::python_framework::register_fundamental_universe(
                py,
                self,
                args.get_item(0)?.extract::<pyo3::Py<pyo3::PyAny>>()?,
            ),
            // Existing rlean custom-universe overload is retained unchanged.
            4 => {
                let source_type = args.get_item(0)?.extract::<String>()?;
                let ticker = args.get_item(1)?.extract::<String>()?;
                let resolution = args.get_item(2)?.extract::<crate::types::Resolution>()?;
                let selector = args.get_item(3)?.extract::<pyo3::Py<pyo3::PyAny>>()?;
                tracing::debug!(
                    "QCAlgorithm.add_universe called for {}:{} at {:?}",
                    source_type,
                    ticker,
                    resolution
                );
                crate::python_framework::register_custom_universe(
                    py,
                    self,
                    source_type,
                    ticker,
                    resolution.into(),
                    selector,
                )
            }
            _ => Err(pyo3::exceptions::PyTypeError::new_err(
                "add_universe expects selector or source_type, ticker, resolution, selector",
            )),
        }
    }

    #[pyo3(name = "set_custom_data_symbols")]
    fn py_set_custom_data_symbols(
        &self,
        source_type: String,
        ticker: String,
        symbols: Vec<String>,
    ) {
        crate::python_framework::set_custom_data_symbols(self, &source_type, &ticker, symbols);
    }

    #[getter(settings)]
    fn py_settings(&self) -> AlgorithmSettingsHandle {
        self.algorithm_settings()
    }

    #[getter(schedule)]
    fn py_schedule(&self) -> ScheduleManagerHandle {
        self.schedule()
    }

    #[getter(date_rules)]
    fn py_date_rules(&self) -> DateRulesHandle {
        self.date_rules()
    }

    #[getter(time_rules)]
    fn py_time_rules(&self) -> TimeRulesHandle {
        self.time_rules()
    }
}

/// SDK-owned service surface for canonical `QCAlgorithm` state mutations.
pub struct AlgorithmApi<'a> {
    algorithm: &'a mut QcAlgorithm,
}

impl<'a> AlgorithmApi<'a> {
    pub fn new(algorithm: &'a mut QcAlgorithm) -> Self {
        Self { algorithm }
    }

    pub fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        self.algorithm.set_start_date(year, month, day);
    }

    pub fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        self.algorithm.set_end_date(year, month, day);
    }

    pub fn set_cash(&mut self, amount: f64) {
        self.algorithm.set_cash(f2d(amount));
    }

    pub fn add_cash(&mut self, amount: f64) {
        let portfolio = self.algorithm.portfolio.clone();
        *portfolio.cash.write() += f2d(amount);
    }
    pub fn cash(&self) -> Price {
        self.algorithm.cash()
    }
    pub fn portfolio_value(&self) -> Price {
        self.algorithm.portfolio_value()
    }

    pub fn set_name(&mut self, name: &str) {
        self.algorithm.name = name.to_string();
    }

    pub fn set_brokerage_model(&mut self, brokerage: BrokerageName, account_type: AccountType) {
        self.algorithm.set_brokerage_model(brokerage, account_type);
    }

    pub fn set_benchmark(&mut self, ticker: &str) {
        self.algorithm.set_benchmark(ticker);
    }

    pub fn has_security(&self, symbol: &Symbol) -> bool {
        self.algorithm.securities.contains(symbol)
    }

    pub fn is_invested(&self, symbol: &Symbol) -> bool {
        self.algorithm.is_invested(symbol)
    }
    pub fn current_time(&self) -> DateTime {
        self.algorithm.time
    }
    pub fn utc_time(&self) -> DateTime {
        self.algorithm.utc_time
    }
    pub fn is_warming_up(&self) -> bool {
        self.algorithm.is_warming_up
    }
    pub fn live_mode(&self) -> bool {
        self.algorithm.live_mode
    }

    pub fn history_end_date(&self) -> chrono::NaiveDate {
        let current = self.algorithm.time.date_utc();
        if current == DateTime::EPOCH.date_utc() {
            self.algorithm.start_date.date_utc()
        } else {
            current
        }
    }

    pub fn current_or_start_time(&self) -> DateTime {
        if self.algorithm.time == DateTime::EPOCH {
            self.algorithm.start_date
        } else {
            self.algorithm.time
        }
    }

    pub fn subscription_resolution_for(&self, symbol: &Symbol) -> Resolution {
        self.algorithm
            .subscription_manager
            .get_all()
            .into_iter()
            .find(|sub| sub.symbol.id.sid == symbol.id.sid && sub.tick_type == TickType::Trade)
            .or_else(|| {
                self.algorithm
                    .subscription_manager
                    .get_all()
                    .into_iter()
                    .find(|sub| sub.symbol.id.sid == symbol.id.sid)
            })
            .map(|sub| sub.resolution)
            .unwrap_or(Resolution::Minute)
    }

    pub fn find_custom_subscription(&self, ticker: &str) -> Option<SubscriptionDataConfig> {
        self.algorithm
            .subscription_manager
            .get_all()
            .into_iter()
            .find(|sub| {
                sub.custom
                    .as_ref()
                    .map(|custom| custom.ticker.eq_ignore_ascii_case(ticker))
                    .unwrap_or(false)
            })
            .map(|sub| (*sub).clone())
    }

    pub fn matching_normalization_mode(
        &self,
        symbol: &Symbol,
        resolution: Option<Resolution>,
        fallback: DataNormalizationMode,
    ) -> DataNormalizationMode {
        let configs: Vec<SubscriptionDataConfig> = self
            .algorithm
            .subscription_manager
            .get_configs_for_symbol(symbol)
            .into_iter()
            .map(|sub| (*sub).clone())
            .collect();
        matching_normalization_mode(&configs, resolution, fallback)
    }

    pub fn set_warm_up_span(&mut self, span: TimeSpan, resolution: Option<Resolution>) {
        self.algorithm.set_warm_up_with_resolution(span, resolution);
    }

    pub fn set_warm_up_int(&mut self, n: i64, resolution: Option<Resolution>) {
        if resolution.is_some() || n > 365 {
            self.algorithm
                .set_warm_up_bars_with_resolution(n.max(0) as usize, resolution);
        } else {
            let nanos = n * 86_400 * 1_000_000_000i64;
            self.algorithm.set_warm_up(TimeSpan::from_nanos(nanos));
        }
    }

    pub fn add_equity_with_normalization(
        &mut self,
        ticker: &str,
        resolution: Resolution,
        normalization_mode: Option<rlean_core::DataNormalizationMode>,
    ) -> Symbol {
        self.algorithm
            .add_equity_with_normalization(ticker, resolution, normalization_mode)
    }

    pub fn add_forex(&mut self, ticker: &str, resolution: Resolution) -> Symbol {
        self.algorithm.add_forex(ticker, resolution)
    }

    pub fn default_market_for_security(&self, security_type: SecurityType) -> Market {
        self.algorithm.default_market_for_security(security_type)
    }

    pub fn add_crypto(&mut self, ticker: &str, market: &Market, resolution: Resolution) -> Symbol {
        self.algorithm.add_crypto(ticker, market, resolution)
    }

    pub fn add_crypto_future(
        &mut self,
        ticker: &str,
        market: &Market,
        resolution: Resolution,
        leverage: Option<f64>,
    ) -> Symbol {
        if let Some(leverage) = leverage {
            let symbol = Symbol::create_crypto_future(ticker, market);
            self.algorithm.register_security_leverage(&symbol, leverage);
        }
        self.algorithm.add_crypto_future(ticker, market, resolution)
    }

    pub fn add_option(&mut self, underlying_ticker: &str, resolution: Resolution) -> Symbol {
        self.algorithm.add_option(underlying_ticker, resolution)
    }

    pub fn add_option_contract(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        self.algorithm.add_option_contract(symbol, resolution)
    }

    pub fn add_option_quote_contract(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        self.algorithm.add_option_quote_contract(symbol, resolution)
    }

    pub fn add_security_symbol(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        self.algorithm.add_security_symbol(symbol, resolution)
    }

    pub fn remove_security(&mut self, symbol: &Symbol, tag: Option<&str>) -> bool {
        self.algorithm.remove_security(symbol, tag)
    }

    pub fn remove_option_contract(&mut self, symbol: &Symbol, tag: Option<&str>) -> bool {
        self.algorithm.remove_option_contract(symbol, tag)
    }

    pub fn add_custom_data(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
    ) -> Symbol {
        self.algorithm
            .add_custom_data(source_type, ticker, resolution, properties)
    }

    pub fn add_custom_universe_data(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
    ) -> Symbol {
        self.algorithm
            .add_custom_universe_data(source_type, ticker, resolution, properties)
    }

    pub fn add_fundamental_universe_data(
        &mut self,
        source_type: &str,
        resolution: Resolution,
    ) -> Symbol {
        self.algorithm
            .add_fundamental_universe_data(source_type, resolution)
    }

    pub fn add_custom_subscription(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
        data_kind: SubscriptionDataKind,
    ) -> Symbol {
        match data_kind {
            SubscriptionDataKind::Custom => {
                self.add_custom_data(source_type, ticker, resolution, properties)
            }
            SubscriptionDataKind::Universe => {
                self.add_custom_universe_data(source_type, ticker, resolution, properties)
            }
            SubscriptionDataKind::FundamentalUniverse => {
                self.add_fundamental_universe_data(source_type, resolution)
            }
            SubscriptionDataKind::Market | SubscriptionDataKind::Option => {
                panic!("custom data registration requires custom or universe kind")
            }
        }
    }

    pub fn set_custom_data_query(
        &mut self,
        source_type: &str,
        ticker: &str,
        query: CustomDataQuery,
    ) -> bool {
        self.algorithm
            .subscription_manager
            .set_custom_dynamic_query(source_type, ticker, query)
    }

    pub fn market_order(
        &mut self,
        symbol: &Symbol,
        quantity: f64,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicket {
        self.algorithm.market_order_with_options(
            symbol,
            f2d(quantity),
            time_in_force,
            outside_regular_trading_hours,
        )
    }

    pub fn limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: f64,
        limit_price: f64,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
        post_only: bool,
    ) -> OrderTicket {
        self.algorithm.limit_order_with_properties(
            symbol,
            f2d(quantity),
            f2d(limit_price),
            time_in_force,
            outside_regular_trading_hours,
            post_only,
        )
    }

    pub fn stop_market_order(
        &mut self,
        symbol: &Symbol,
        quantity: f64,
        stop_price: f64,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicket {
        self.algorithm.stop_market_order_with_options(
            symbol,
            f2d(quantity),
            f2d(stop_price),
            time_in_force,
            outside_regular_trading_hours,
        )
    }

    pub fn market_on_open_order(&mut self, symbol: &Symbol, quantity: f64) -> OrderTicket {
        self.algorithm.market_on_open_order(symbol, f2d(quantity))
    }

    pub fn market_on_close_order(&mut self, symbol: &Symbol, quantity: f64) -> OrderTicket {
        self.algorithm.market_on_close_order(symbol, f2d(quantity))
    }

    pub fn set_holdings(&mut self, symbol: &Symbol, target: f64) {
        self.algorithm.set_holdings(symbol, f2d(target));
    }

    pub fn liquidate(&mut self, symbol: Option<&Symbol>) {
        self.algorithm.liquidate(symbol);
    }
}

pub fn matching_normalization_mode(
    configs: &[SubscriptionDataConfig],
    resolution: Option<Resolution>,
    fallback: DataNormalizationMode,
) -> DataNormalizationMode {
    configs
        .iter()
        .find(|config| {
            config.tick_type == TickType::Trade
                && resolution
                    .map(|resolution| config.resolution == resolution)
                    .unwrap_or(true)
        })
        .or_else(|| {
            configs
                .iter()
                .find(|config| config.tick_type == TickType::Trade)
        })
        .or_else(|| configs.first())
        .map(|config| config.normalization_mode)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_algorithm::lifecycle::{AlgorithmHistoryService, HistoryColumns};
    use rlean_data_tables::{TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::sync::RwLock;

    struct TestRuntimeServices {
        history_service: Arc<dyn AlgorithmHistoryService>,
        runtime_parameters: Arc<RwLock<HashMap<String, String>>>,
        registered_indicators: RegisteredIndicatorRegistry,
    }

    impl Default for TestRuntimeServices {
        fn default() -> Self {
            Self {
                history_service: Arc::new(TestHistoryService),
                runtime_parameters: Arc::new(RwLock::new(HashMap::new())),
                registered_indicators: Arc::new(Mutex::new(HashMap::new())),
            }
        }
    }

    struct TestHistoryService;

    impl AlgorithmHistoryService for TestHistoryService {
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

    impl AlgorithmRuntimeServices for TestRuntimeServices {
        fn history_service(&self) -> Arc<dyn AlgorithmHistoryService> {
            self.history_service.clone()
        }

        fn runtime_parameters(&self) -> Arc<RwLock<HashMap<String, String>>> {
            self.runtime_parameters.clone()
        }

        fn registered_indicators(&self) -> RegisteredIndicatorRegistry {
            self.registered_indicators.clone()
        }
    }

    fn update_registered_indicators(
        registry: &RegisteredIndicatorRegistry,
        slice: &rlean_data::Slice,
    ) {
        let registry = registry
            .lock()
            .expect("registered indicator registry poisoned");
        for (sid, indicators) in registry.iter() {
            let Some(bar) = slice.bars.get(sid) else {
                continue;
            };
            for indicator in indicators {
                indicator.update_bar(bar);
            }
        }
    }

    #[test]
    fn registered_sma_updates_from_slice_without_python_update() {
        let runtime_services = Arc::new(TestRuntimeServices::default());
        let registry = runtime_services.registered_indicators();
        let context = AlgorithmConstructionContext::new_with_runtime_services(
            Arc::new(Mutex::new(QcAlgorithm::new("Algorithm", dec!(100000)))),
            runtime_services,
        );
        let algorithm =
            AlgorithmHandle::with_default_context(context, AlgorithmHandle::default_algorithm);
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let sma = algorithm.sma(
            SymbolHandle::new(symbol.clone()),
            2,
            Some(Resolution::Minute),
        );

        let first_time = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(9, 31, 0)
                .unwrap(),
        );
        let mut first = rlean_data::Slice::new(first_time);
        first.add_bar(TradeBar::new(
            symbol.clone(),
            first_time,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(10), dec!(100)),
        ));
        update_registered_indicators(&registry, &first);

        assert!(!sma.is_ready());
        assert_eq!(sma.samples(), 1);

        let second_time = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(9, 32, 0)
                .unwrap(),
        );
        let mut second = rlean_data::Slice::new(second_time);
        second.add_bar(TradeBar::new(
            symbol,
            second_time,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(20), dec!(100)),
        ));
        update_registered_indicators(&registry, &second);

        assert!(sma.is_ready());
        assert_eq!(sma.samples(), 2);
        assert_eq!(sma.current().value(), 15.0);
    }
}
