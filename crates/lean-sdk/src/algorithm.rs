pub use lean_algorithm::lifecycle::{
    AlgorithmBridge, AlgorithmRuntimeServices, AlgorithmStateAccess, OptionSubscription,
    RegisteredIndicatorBridge, RegisteredIndicatorRegistry, UniverseSelection,
};
use lean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
use lean_core::{
    DataNormalizationMode, DateTime, Market, Price, Resolution, SecurityType, Symbol, TickType,
    TimeSpan,
};
use lean_data::{CustomDataQuery, SubscriptionDataConfig, SubscriptionDataKind};
use lean_orders::order::TimeInForce;
use lean_orders::OrderTicket;
use lean_sdk_annotations::{sdk_bind, sdk_getter, sdk_method, sdk_new};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::data::ns_to_exchange_naive;
use crate::indicators::{
    AverageTrueRange, BollingerBandsIndicator, ExponentialMovingAverage, IdentityIndicator,
    MacdIndicator, MomentumPercentIndicator, RelativeStrengthIndex, SimpleMovingAverage,
    StandardDeviationIndicator,
};
use crate::orders::{OrderTicketContext, OrderTicketHandle};
use crate::portfolio::PortfolioView;
use crate::securities::{OptionSecurityHandle, SecurityHandle, SymbolHandle};
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

#[sdk_bind(py_name = "QCAlgorithm", subclass)]
#[derive(Clone)]
pub struct AlgorithmHandle {
    inner: Arc<Mutex<QcAlgorithm>>,
    universe_settings: Arc<Mutex<UniverseSettings>>,
    runtime_services: Arc<dyn AlgorithmRuntimeServices>,
}

#[derive(Clone)]
pub struct AlgorithmConstructionContext {
    state: Arc<Mutex<QcAlgorithm>>,
    runtime_services: Arc<dyn AlgorithmRuntimeServices>,
}

impl AlgorithmConstructionContext {
    pub fn new_with_runtime_services(
        state: Arc<Mutex<QcAlgorithm>>,
        runtime_services: Arc<dyn AlgorithmRuntimeServices>,
    ) -> Self {
        Self {
            state,
            runtime_services,
        }
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
    #[sdk_new]
    pub fn default_algorithm() -> Self {
        if let Some(context) = DEFAULT_ALGORITHM_CONTEXT.with(|state| state.borrow().clone()) {
            return Self {
                inner: context.state,
                universe_settings: Arc::new(Mutex::new(UniverseSettings::default())),
                runtime_services: context.runtime_services,
            };
        }

        panic!("QCAlgorithm must be constructed inside an AlgorithmConstructionContext")
    }

    pub fn inner(&self) -> Arc<Mutex<QcAlgorithm>> {
        self.inner.clone()
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

    #[sdk_getter]
    pub fn portfolio(&self) -> PortfolioView {
        PortfolioView::from_algorithm(self.inner.clone())
    }

    #[sdk_getter]
    pub fn universe_settings(&self) -> UniverseSettingsHandle {
        UniverseSettingsHandle::from_shared(self.universe_settings.clone())
    }

    #[sdk_getter]
    pub fn date_rules(&self) -> DateRulesHandle {
        DateRulesHandle::new()
    }

    #[sdk_getter]
    pub fn time_rules(&self) -> TimeRulesHandle {
        TimeRulesHandle::new()
    }

    fn order_ticket_context(&self) -> OrderTicketContext {
        let algorithm = self.inner.clone();
        let transactions = self.inner.lock().unwrap().transactions.clone();
        OrderTicketContext::new(transactions, move || algorithm.lock().unwrap().utc_time)
    }

    #[sdk_method]
    pub fn set_start_date(&self, year: i32, month: u32, day: u32) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_start_date(year, month, day);
    }

    #[sdk_method]
    pub fn set_end_date(&self, year: i32, month: u32, day: u32) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_end_date(year, month, day);
    }

    #[sdk_method]
    pub fn set_cash(&self, amount: f64) {
        let mut algorithm = self.inner.lock().unwrap();
        AlgorithmApi::new(&mut algorithm).set_cash(amount);
        let cash = algorithm.cash();
        *algorithm.portfolio.cash.write() = cash;
    }

    #[sdk_method]
    pub fn add_cash(&self, amount: f64) {
        let mut algorithm = self.inner.lock().unwrap();
        AlgorithmApi::new(&mut algorithm).add_cash(amount);
        let cash = algorithm.cash();
        *algorithm.portfolio.cash.write() = cash;
    }

    #[sdk_getter]
    pub fn cash(&self) -> f64 {
        decimal_to_f64(AlgorithmApi::new(&mut self.inner.lock().unwrap()).cash())
    }

    #[sdk_getter(py_name = "portfolio_value")]
    pub fn portfolio_value(&self) -> f64 {
        decimal_to_f64(AlgorithmApi::new(&mut self.inner.lock().unwrap()).portfolio_value())
    }

    #[sdk_method]
    pub fn set_name(&self, name: String) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_name(&name);
    }

    #[sdk_method]
    pub fn set_brokerage_model(&self, brokerage: BrokerageName, account_type: AccountType) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .set_brokerage_model(brokerage, account_type);
    }

    #[sdk_method]
    pub fn set_benchmark(&self, ticker: String) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_benchmark(&ticker);
    }

    #[sdk_method]
    pub fn get_parameter(&self, key: String, default: Option<String>) -> Option<String> {
        self.runtime_services
            .runtime_parameters()
            .read()
            .expect("runtime parameters poisoned")
            .get(&key)
            .cloned()
            .or(default)
    }

    #[sdk_method]
    pub fn set_parameter(&self, key: String, value: String) {
        self.runtime_services
            .runtime_parameters()
            .write()
            .expect("runtime parameters poisoned")
            .insert(key, value);
    }

    #[sdk_method]
    pub fn has_security(&self, symbol: SymbolHandle) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).has_security(symbol.inner())
    }

    #[sdk_method]
    pub fn is_invested(&self, symbol: SymbolHandle) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).is_invested(symbol.inner())
    }

    #[sdk_getter(py_name = "time")]
    pub fn current_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .current_time()
                .0,
        )
    }

    #[sdk_getter]
    pub fn utc_time(&self) -> chrono::NaiveDateTime {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .utc_time()
            .to_utc()
            .naive_utc()
    }

    #[sdk_getter]
    pub fn is_warming_up(&self) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).is_warming_up()
    }

    #[sdk_method]
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

    #[sdk_method]
    pub fn set_warm_up_int(&self, n: i64, resolution: Option<Resolution>) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_warm_up_int(n, resolution);
    }

    #[sdk_method]
    pub fn set_warm_up(&self, n: i64, resolution: Option<Resolution>) {
        self.set_warm_up_int(n, resolution);
    }

    #[sdk_method]
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
        SecurityHandle::new(symbol)
    }

    #[sdk_method]
    pub fn add_forex(&self, ticker: String, resolution: Resolution) -> SymbolHandle {
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap()).add_forex(&ticker, resolution),
        )
    }

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
    pub fn add_security(
        &self,
        security_type: SecurityType,
        ticker: String,
        resolution: Resolution,
    ) -> SymbolHandle {
        let market = Market::usa();
        let symbol = Symbol::create_with_security_type(&ticker, security_type, Some(market));
        SymbolHandle::new(
            AlgorithmApi::new(&mut self.inner.lock().unwrap())
                .add_security_symbol(symbol, resolution),
        )
    }

    #[sdk_method]
    pub fn add_option(
        &self,
        underlying_ticker: String,
        resolution: Resolution,
    ) -> OptionSecurityHandle {
        let symbol = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .add_option(&underlying_ticker, resolution);
        OptionSecurityHandle::new(symbol, self.inner.clone())
    }

    #[sdk_method]
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

    #[sdk_method]
    pub fn add_data(
        &self,
        source_type: String,
        ticker: String,
        resolution: Resolution,
        properties: Option<HashMap<String, String>>,
    ) -> SecurityHandle {
        SecurityHandle::new(self.inner.lock().unwrap().add_custom_data(
            &source_type,
            &ticker,
            resolution,
            properties.unwrap_or_default(),
        ))
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

    #[sdk_method]
    pub fn log(&self, message: String) {
        self.inner.lock().unwrap().log_message(message);
    }

    #[sdk_method]
    pub fn debug(&self, message: String) {
        self.inner.lock().unwrap().debug(message);
    }

    #[sdk_method]
    pub fn error(&self, message: String) {
        self.inner.lock().unwrap().error(message);
    }

    #[sdk_method]
    pub fn remove_security(&self, symbol: SymbolHandle, tag: Option<String>) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .remove_security(symbol.inner(), tag.as_deref())
    }

    #[sdk_method]
    pub fn remove_option_contract(&self, symbol: SymbolHandle, tag: Option<String>) -> bool {
        AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .remove_option_contract(symbol.inner(), tag.as_deref())
    }

    #[sdk_method]
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

    #[sdk_method]
    pub fn buy(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        self.market_order(symbol, quantity.abs(), None, None)
    }

    #[sdk_method]
    pub fn sell(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        self.market_order(symbol, -quantity.abs(), None, None)
    }

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
    pub fn market_on_open_order(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .market_on_open_order(symbol.inner(), quantity);
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    #[sdk_method]
    pub fn market_on_close_order(&self, symbol: SymbolHandle, quantity: f64) -> OrderTicketHandle {
        let ticket = AlgorithmApi::new(&mut self.inner.lock().unwrap())
            .market_on_close_order(symbol.inner(), quantity);
        OrderTicketHandle::from_ticket(ticket, self.order_ticket_context())
    }

    #[sdk_method]
    pub fn set_holdings(&self, symbol: SymbolHandle, target: f64) {
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).set_holdings(symbol.inner(), target);
    }

    #[sdk_method]
    pub fn liquidate(&self, symbol: Option<SymbolHandle>) {
        let symbol = symbol.as_ref().map(SymbolHandle::inner);
        AlgorithmApi::new(&mut self.inner.lock().unwrap()).liquidate(symbol);
    }

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
    pub fn identity(
        &self,
        symbol: SymbolHandle,
        _resolution: Option<Resolution>,
    ) -> IdentityIndicator {
        let indicator = IdentityIndicator::new();
        self.register_indicator_handle(&symbol, indicator.registered_handle());
        indicator
    }

    #[sdk_method]
    pub fn register_indicator(
        &self,
        _symbol: SymbolHandle,
        _indicator: IdentityIndicator,
        _resolution: Option<Resolution>,
    ) {
    }

    #[sdk_method]
    pub fn warm_up_indicator(
        &self,
        _symbol: SymbolHandle,
        _indicator: IdentityIndicator,
        _resolution: Option<Resolution>,
    ) {
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

    #[sdk_method]
    pub fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        self.algorithm.set_start_date(year, month, day);
    }

    #[sdk_method]
    pub fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        self.algorithm.set_end_date(year, month, day);
    }

    #[sdk_method]
    pub fn set_cash(&mut self, amount: f64) {
        self.algorithm.set_cash(f2d(amount));
    }

    #[sdk_method]
    pub fn add_cash(&mut self, amount: f64) {
        let portfolio = self.algorithm.portfolio.clone();
        *portfolio.cash.write() += f2d(amount);
    }

    #[sdk_getter]
    pub fn cash(&self) -> Price {
        self.algorithm.cash()
    }

    #[sdk_getter(py_name = "portfolio_value")]
    pub fn portfolio_value(&self) -> Price {
        self.algorithm.portfolio_value()
    }

    #[sdk_method]
    pub fn set_name(&mut self, name: &str) {
        self.algorithm.name = name.to_string();
    }

    #[sdk_method]
    pub fn set_brokerage_model(&mut self, brokerage: BrokerageName, account_type: AccountType) {
        self.algorithm.set_brokerage_model(brokerage, account_type);
    }

    #[sdk_method]
    pub fn set_benchmark(&mut self, ticker: &str) {
        self.algorithm.set_benchmark(ticker);
    }

    #[sdk_method]
    pub fn has_security(&self, symbol: &Symbol) -> bool {
        self.algorithm.securities.contains(symbol)
    }

    #[sdk_method]
    pub fn is_invested(&self, symbol: &Symbol) -> bool {
        self.algorithm.is_invested(symbol)
    }

    #[sdk_getter(py_name = "time")]
    pub fn current_time(&self) -> DateTime {
        self.algorithm.time
    }

    #[sdk_getter]
    pub fn utc_time(&self) -> DateTime {
        self.algorithm.utc_time
    }

    #[sdk_getter]
    pub fn is_warming_up(&self) -> bool {
        self.algorithm.is_warming_up
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

    #[sdk_method]
    pub fn set_warm_up_span(&mut self, span: TimeSpan, resolution: Option<Resolution>) {
        self.algorithm.set_warm_up_with_resolution(span, resolution);
    }

    #[sdk_method]
    pub fn set_warm_up_int(&mut self, n: i64, resolution: Option<Resolution>) {
        if resolution.is_some() || n > 365 {
            self.algorithm
                .set_warm_up_bars_with_resolution(n.max(0) as usize, resolution);
        } else {
            let nanos = n * 86_400 * 1_000_000_000i64;
            self.algorithm.set_warm_up(TimeSpan::from_nanos(nanos));
        }
    }

    #[sdk_method]
    pub fn add_equity_with_normalization(
        &mut self,
        ticker: &str,
        resolution: Resolution,
        normalization_mode: Option<lean_core::DataNormalizationMode>,
    ) -> Symbol {
        self.algorithm
            .add_equity_with_normalization(ticker, resolution, normalization_mode)
    }

    #[sdk_method]
    pub fn add_forex(&mut self, ticker: &str, resolution: Resolution) -> Symbol {
        self.algorithm.add_forex(ticker, resolution)
    }

    pub fn default_market_for_security(&self, security_type: SecurityType) -> Market {
        self.algorithm.default_market_for_security(security_type)
    }

    #[sdk_method]
    pub fn add_crypto(&mut self, ticker: &str, market: &Market, resolution: Resolution) -> Symbol {
        self.algorithm.add_crypto(ticker, market, resolution)
    }

    #[sdk_method]
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

    #[sdk_method]
    pub fn add_option(&mut self, underlying_ticker: &str, resolution: Resolution) -> Symbol {
        self.algorithm.add_option(underlying_ticker, resolution)
    }

    #[sdk_method]
    pub fn add_option_contract(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        self.algorithm.add_option_contract(symbol, resolution)
    }

    #[sdk_method]
    pub fn add_security_symbol(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        self.algorithm.add_security_symbol(symbol, resolution)
    }

    #[sdk_method]
    pub fn remove_security(&mut self, symbol: &Symbol, tag: Option<&str>) -> bool {
        self.algorithm.remove_security(symbol, tag)
    }

    #[sdk_method]
    pub fn remove_option_contract(&mut self, symbol: &Symbol, tag: Option<&str>) -> bool {
        self.algorithm.remove_option_contract(symbol, tag)
    }

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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
            SubscriptionDataKind::Market | SubscriptionDataKind::Option => {
                panic!("custom data registration requires custom or universe kind")
            }
        }
    }

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
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

    #[sdk_method]
    pub fn market_on_open_order(&mut self, symbol: &Symbol, quantity: f64) -> OrderTicket {
        self.algorithm.market_on_open_order(symbol, f2d(quantity))
    }

    #[sdk_method]
    pub fn market_on_close_order(&mut self, symbol: &Symbol, quantity: f64) -> OrderTicket {
        self.algorithm.market_on_close_order(symbol, f2d(quantity))
    }

    #[sdk_method]
    pub fn set_holdings(&mut self, symbol: &Symbol, target: f64) {
        self.algorithm.set_holdings(symbol, f2d(target));
    }

    #[sdk_method]
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
    use lean_algorithm::lifecycle::{AlgorithmHistoryService, HistoryColumns};
    use lean_data::{TradeBar, TradeBarData};
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
        slice: &lean_data::Slice,
    ) {
        let registry = registry.lock().expect("registered indicator registry poisoned");
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
        let algorithm = AlgorithmHandle::with_default_context(context, AlgorithmHandle::default_algorithm);
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let sma = algorithm.sma(SymbolHandle::new(symbol.clone()), 2, Some(Resolution::Minute));

        let first_time = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(9, 31, 0)
                .unwrap(),
        );
        let mut first = lean_data::Slice::new(first_time);
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
        let mut second = lean_data::Slice::new(second_time);
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
