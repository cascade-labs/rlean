use crate::{
    algorithm::AlgorithmStatus, logging::AlgorithmLogging, portfolio::SecurityPortfolioManager,
    runtime_statistics::RuntimeStatistics, securities::SecurityManager, BuyingPowerModel,
};
use chrono::Timelike;
use rlean_core::{
    DataNormalizationMode, DateTime, Market, MarketHoursDatabase, OptionRight, OptionStyle, Price,
    Quantity, Resolution, SecurityType, SettlementType, Symbol, SymbolOptionsExt, SymbolProperties,
    TimeSpan,
};
use rlean_data::{
    subscription::{CustomSubscriptionMetadata, FundamentalUniverseSubscriptionMetadata},
    SubscriptionDataConfig, SubscriptionManager,
};
use rlean_options::OptionChain;
use rlean_orders::{
    combo_orders::{ComboLegDetails, ComboLegLimitOrder, ComboLimitOrder, ComboMarketOrder},
    fee_model::{
        BybitFeeModel, FeeModel, FlatFeeModel, HyperliquidFeeModel, InteractiveBrokersFeeModel,
        OrderFee, OrderFeeParameters, TradierFeeModel,
    },
    order::{Order, OrderStatus, OrderSubmissionData, OrderType, TimeInForce},
    order_event::OrderEvent,
    order_ticket::OrderTicket,
    trailing_stop_order::TrailingStopOrderParams,
    transaction_manager::TransactionManager,
    LimitIfTouchedOrder, TrailingStopOrder,
};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::Arc;

/// LEAN-style option universe filter configured through `Option.set_filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OptionFilter {
    pub min_strike_rank: i32,
    pub max_strike_rank: i32,
    pub min_expiry_days: i32,
    pub max_expiry_days: i32,
}

impl Default for OptionFilter {
    fn default() -> Self {
        Self {
            min_strike_rank: -1,
            max_strike_rank: 1,
            min_expiry_days: 0,
            max_expiry_days: 35,
        }
    }
}

fn tradier_extended_session(time: DateTime) -> Option<&'static str> {
    let local = time.to_tz(rlean_core::time::tz::NEW_YORK);
    let seconds = local.num_seconds_from_midnight();
    match seconds {
        14_400..33_840 => Some("pre"),
        57_600..71_700 => Some("post"),
        _ => None,
    }
}

fn infer_quote_currency(ticker: &str) -> Option<String> {
    let upper = ticker.to_ascii_uppercase();
    for quote in ["USDT", "USDC", "USD", "BTC", "ETH"] {
        if upper.ends_with(quote) && upper.len() > quote.len() {
            return Some(quote.to_string());
        }
    }
    None
}

fn infer_base_currency(symbol: &Symbol, quote_currency: &str) -> Option<String> {
    match symbol.security_type() {
        SecurityType::Crypto | SecurityType::CryptoFuture => {
            let upper = symbol.value.to_ascii_uppercase();
            let quote = quote_currency.to_ascii_uppercase();
            if upper.ends_with(&quote) && upper.len() > quote.len() {
                Some(upper[..upper.len() - quote.len()].to_string())
            } else {
                Some(upper)
            }
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountType {
    Margin,
    Cash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerageName {
    Default,
    QuantConnectBrokerage,
    InteractiveBrokersBrokerage,
    TradierBrokerage,
    HyperliquidBrokerage,
}

/// Represents an open option position held by the algorithm.
#[derive(Debug, Clone)]
pub struct OpenOptionPosition {
    pub symbol: Symbol,
    pub strike: rust_decimal::Decimal,
    pub expiry: chrono::NaiveDate,
    pub right: OptionRight,
    pub style: OptionStyle,
    pub settlement: SettlementType,
    /// Negative quantity = short position.
    pub quantity: rust_decimal::Decimal,
    /// Premium received (short) or paid (long) per contract.
    pub entry_price: rust_decimal::Decimal,
    /// Number of shares per contract (usually 100).
    pub contract_unit_of_trade: i64,
}

/// The base algorithm class. Provides all the helper methods that C# QCAlgorithm does.
/// User strategies embed or extend this via trait implementations.
pub struct QcAlgorithm {
    pub name: String,
    pub start_date: DateTime,
    pub end_date: DateTime,
    pub status: AlgorithmStatus,

    // Core state
    pub portfolio: Arc<SecurityPortfolioManager>,
    pub securities: SecurityManager,
    pub transactions: Arc<TransactionManager>,
    pub subscription_manager: SubscriptionManager,

    // Current time (updated each bar)
    pub time: DateTime,
    pub utc_time: DateTime,

    // Logging
    pub log: AlgorithmLogging,

    // Runtime statistics
    pub statistics: RuntimeStatistics,

    // Warm-up config
    pub warmup_period: Option<TimeSpan>,
    pub warmup_bar_count: Option<usize>,
    pub warmup_duration: Option<TimeSpan>,
    pub warmup_resolution: Option<Resolution>,
    pub is_warming_up: bool,

    // Order counter
    order_id_counter: i64,

    // Option tracking
    /// Canonical option symbols (e.g. `?SPY`) for chain subscriptions.
    pub option_subscriptions: Vec<Symbol>,
    /// Resolution requested for each canonical option subscription.
    pub option_subscription_resolutions: HashMap<String, Resolution>,
    /// LEAN-style option filters keyed by canonical option permtick.
    pub option_filters: HashMap<String, OptionFilter>,
    /// Monotonic version stamp bumped whenever the option-subscription set,
    /// resolutions, or filters change. Combined with
    /// `subscription_manager.generation()` to let the subscription-sync loop
    /// skip its diff when nothing changed (issue #64). Option state lives on the
    /// algorithm rather than the `SubscriptionManager`, so it needs its own
    /// stamp to be covered by the fast-path skip.
    pub option_subscriptions_generation: u64,
    /// Specific option contracts that have been subscribed to.
    pub open_option_contracts: Vec<Symbol>,
    /// Generated option chains keyed by canonical ticker (e.g. "?SPY").
    pub option_chains: HashMap<String, OptionChain>,

    /// The benchmark symbol set by the algorithm (ticker, e.g. "SPY").
    /// When None, the runner defaults to SPY automatically.
    pub benchmark_symbol: Option<String>,
    /// Dated annual risk-free model used by statistics and option valuation.
    pub risk_free_interest_rate_model: Arc<dyn rlean_core::RiskFreeInterestRateModel>,

    pub brokerage_name: BrokerageName,
    pub account_type: AccountType,
    /// Fixed cash amount excluded from portfolio target sizing. When unset,
    /// `free_portfolio_value_percentage` trails total portfolio value.
    pub free_portfolio_value: Option<Decimal>,
    /// C# LEAN defaults to reserving 0.25% for SetHoldings/framework targets.
    pub free_portfolio_value_percentage: Decimal,
    /// C# LEAN suppresses target adjustments below 0.1% of portfolio value.
    pub minimum_order_margin_portfolio_percentage: Decimal,
    pub market_hours_database: Arc<MarketHoursDatabase>,
    security_leverage_overrides: HashMap<u64, f64>,
}

impl QcAlgorithm {
    pub fn new(name: impl Into<String>, starting_cash: Price) -> Self {
        QcAlgorithm {
            name: name.into(),
            start_date: DateTime::EPOCH,
            end_date: DateTime::MAX,
            status: AlgorithmStatus::Initializing,
            portfolio: Arc::new(SecurityPortfolioManager::new(starting_cash)),
            securities: SecurityManager::new(),
            transactions: Arc::new(TransactionManager::new()),
            subscription_manager: SubscriptionManager::new(),
            time: DateTime::EPOCH,
            utc_time: DateTime::EPOCH,
            log: AlgorithmLogging::default(),
            statistics: RuntimeStatistics::default(),
            warmup_period: None,
            warmup_bar_count: None,
            warmup_duration: None,
            warmup_resolution: None,
            is_warming_up: false,
            order_id_counter: 0,
            option_subscriptions: Vec::new(),
            option_subscription_resolutions: HashMap::new(),
            option_filters: HashMap::new(),
            option_subscriptions_generation: 0,
            open_option_contracts: Vec::new(),
            option_chains: HashMap::new(),
            benchmark_symbol: None,
            risk_free_interest_rate_model: Arc::new(
                rlean_core::ConstantRiskFreeInterestRateModel::new(dec!(0.01)),
            ),
            brokerage_name: BrokerageName::Default,
            account_type: AccountType::Margin,
            free_portfolio_value: None,
            free_portfolio_value_percentage: dec!(0.0025),
            minimum_order_margin_portfolio_percentage: dec!(0.001),
            market_hours_database: MarketHoursDatabase::global(),
            security_leverage_overrides: HashMap::new(),
        }
    }

    pub fn set_market_hours_database(&mut self, market_hours_database: Arc<MarketHoursDatabase>) {
        self.market_hours_database = market_hours_database;
    }

    /// C# LEAN `SecurityPortfolioManager.TotalPortfolioValueLessFreeBuffer`.
    pub fn portfolio_value_less_free_buffer(&self) -> Decimal {
        let total = self.portfolio_value();
        let free = self
            .free_portfolio_value
            .unwrap_or(total * self.free_portfolio_value_percentage);
        total - free
    }

    pub fn set_brokerage_model(&mut self, brokerage: BrokerageName, account_type: AccountType) {
        if brokerage == BrokerageName::HyperliquidBrokerage && account_type != AccountType::Margin {
            panic!("HyperliquidBrokerage only supports margin accounts");
        }
        self.brokerage_name = brokerage;
        self.account_type = account_type;
        for security in self.securities.all() {
            self.initialize_security_models(security);
        }
    }

    pub fn default_market_for_security(&self, security_type: SecurityType) -> Market {
        match (self.brokerage_name, security_type) {
            (BrokerageName::HyperliquidBrokerage, SecurityType::CryptoFuture) => {
                Market::hyperliquid()
            }
            (_, SecurityType::Equity | SecurityType::Option | SecurityType::IndexOption) => {
                Market::usa()
            }
            (_, SecurityType::Forex) => Market::forex(),
            (_, SecurityType::Crypto | SecurityType::CryptoFuture) => Market::binance(),
            _ => Market::usa(),
        }
    }

    pub fn default_leverage_for_security(&self, symbol: &Symbol) -> f64 {
        if self.account_type == AccountType::Cash {
            return 1.0;
        }
        if let Some(leverage) = self.security_leverage_overrides.get(&symbol.id.sid) {
            return *leverage;
        }
        if symbol.security_type() == SecurityType::CryptoFuture
            && symbol.market().as_str() == Market::HYPERLIQUID
        {
            panic!(
                "Hyperliquid CryptoFuture {} requires maxLeverage metadata before security initialization",
                symbol.value
            );
        }
        match symbol.security_type() {
            SecurityType::Equity => 2.0,
            SecurityType::Forex | SecurityType::Cfd => 50.0,
            SecurityType::CryptoFuture => 25.0,
            _ => 1.0,
        }
    }

    pub fn register_security_leverage(&mut self, symbol: &Symbol, leverage: f64) {
        BuyingPowerModel::validate_leverage(leverage);
        self.security_leverage_overrides
            .insert(symbol.id.sid, leverage);
        if let Some(security) = self.securities.get(symbol) {
            security.set_leverage(leverage);
        }
    }

    fn initialize_security_models(&self, security: &crate::securities::Security) {
        let model =
            BuyingPowerModel::default_for(&security.symbol, self.account_type == AccountType::Cash);
        security.set_buying_power_model(model);
        security.set_leverage(self.default_leverage_for_security(&security.symbol));
    }

    /// Set the benchmark symbol (e.g. "SPY"). When not called, the runner
    /// automatically uses SPY as the default benchmark.
    pub fn set_benchmark(&mut self, ticker: impl Into<String>) {
        self.benchmark_symbol = Some(ticker.into().to_uppercase());
    }

    pub fn set_risk_free_interest_rate_model(
        &mut self,
        model: Arc<dyn rlean_core::RiskFreeInterestRateModel>,
    ) {
        self.risk_free_interest_rate_model = model;
    }

    pub fn risk_free_interest_rate(&self, date: DateTime) -> Decimal {
        self.risk_free_interest_rate_model.get_interest_rate(date)
    }

    // ─── Configuration ──────────────────────────────────────────────────────

    pub fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(year, month, day).expect("invalid date");
        use chrono::{TimeZone, Utc};
        let dt = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
        self.start_date = DateTime::from(dt);
    }

    pub fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        use chrono::NaiveDate;
        let date = NaiveDate::from_ymd_opt(year, month, day).expect("invalid date");
        use chrono::{TimeZone, Utc};
        let dt = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
        self.end_date = DateTime::from(dt);
    }

    pub fn set_cash(&self, amount: Price) {
        *self.portfolio.cash.write() = amount;
        self.portfolio.set_starting_cash(amount);
    }

    pub fn set_warmup(&mut self, period: TimeSpan) {
        self.warmup_period = Some(period);
        self.warmup_bar_count = None;
        self.warmup_duration = None;
        self.warmup_resolution = None;
    }

    pub fn set_warmup_periods(&mut self, periods: i64, resolution: Resolution) {
        let nanos = resolution
            .to_nanos()
            .unwrap_or(TimeSpan::ONE_DAY.nanos as u64) as i64
            * periods;
        self.warmup_period = Some(TimeSpan::from_nanos(nanos));
        self.warmup_bar_count = None;
        self.warmup_duration = None;
        self.warmup_resolution = Some(resolution);
    }

    /// Set warm-up by number of bars. During warm-up `on_data` is called but
    /// orders are not processed and equity is not recorded.
    pub fn set_warm_up_bars(&mut self, bar_count: usize) {
        self.set_warm_up_bars_with_resolution(bar_count, None);
    }

    pub fn set_warm_up_bars_with_resolution(
        &mut self,
        bar_count: usize,
        resolution: Option<Resolution>,
    ) {
        self.warmup_period = None;
        self.warmup_bar_count = Some(bar_count);
        self.warmup_duration = None;
        self.warmup_resolution = resolution;
        self.is_warming_up = true;
    }

    /// Set warm-up by time period.
    pub fn set_warm_up(&mut self, duration: TimeSpan) {
        self.set_warm_up_with_resolution(duration, None);
    }

    pub fn set_warm_up_with_resolution(
        &mut self,
        duration: TimeSpan,
        resolution: Option<Resolution>,
    ) {
        self.warmup_period = None;
        self.warmup_bar_count = None;
        self.warmup_duration = Some(duration);
        self.warmup_resolution = resolution;
        self.is_warming_up = true;
    }

    /// Called by the engine when warm-up data has been fully replayed.
    pub fn end_warm_up(&mut self) {
        self.is_warming_up = false;
        self.warmup_period = None;
        self.warmup_bar_count = None;
        self.warmup_duration = None;
        self.warmup_resolution = None;
    }

    // ─── Universe Management ─────────────────────────────────────────────────

    pub fn add_equity(&mut self, ticker: &str, resolution: Resolution) -> Symbol {
        self.add_equity_with_normalization(
            ticker,
            resolution,
            Some(DataNormalizationMode::Adjusted),
        )
    }

    /// Equivalent to C# Lean's `AddEquity(..., DataNormalizationMode? dataNormalizationMode = null)`.
    /// When `normalization_mode` is `None`, the default `Adjusted` is used; the
    /// Python wrapper passes the configured `UniverseSettings.DataNormalizationMode`
    /// to mirror the LEAN universe-settings fallback.
    pub fn add_equity_with_normalization(
        &mut self,
        ticker: &str,
        resolution: Resolution,
        normalization_mode: Option<DataNormalizationMode>,
    ) -> Symbol {
        let market = Market::usa();
        let symbol = Symbol::create_equity(ticker, &market);
        self.add_equity_symbol(
            symbol,
            resolution,
            normalization_mode.unwrap_or(DataNormalizationMode::Adjusted),
        )
    }

    fn add_equity_symbol(
        &mut self,
        symbol: Symbol,
        resolution: Resolution,
        normalization_mode: DataNormalizationMode,
    ) -> Symbol {
        self.add_equity_subscriptions(symbol.clone(), resolution, normalization_mode);

        // Idempotent: if the security already exists (e.g. called again during
        // universe rebalancing), keep it as-is so the runner-updated price is
        // not reset to zero.
        if self.securities.contains(&symbol) {
            return symbol;
        }

        let hours = self.market_hours_database.exchange_hours(&symbol);
        let props = SymbolProperties::default();
        let security = crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
            self.portfolio.holdings_store(),
        );
        self.initialize_security_models(&security);
        self.securities.add(security);
        symbol
    }

    pub fn add_security_symbol(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        match symbol.security_type() {
            SecurityType::Base => self.add_base_symbol(symbol, resolution),
            SecurityType::Equity => {
                self.add_equity_symbol(symbol, resolution, DataNormalizationMode::Adjusted)
            }
            SecurityType::Forex => self.add_forex(&symbol.value, resolution),
            SecurityType::Crypto => {
                let market = symbol.market().clone();
                self.add_crypto(&symbol.value, &market, resolution)
            }
            SecurityType::CryptoFuture => {
                let market = symbol.market().clone();
                self.add_crypto_future(&symbol.value, &market, resolution)
            }
            SecurityType::Option => {
                self.ensure_option_security(&symbol, resolution);
                symbol
            }
            other => panic!("Universe selection does not support {other:?} securities yet"),
        }
    }

    pub fn add_custom_data(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
    ) -> Symbol {
        let market = Market::usa();
        let symbol = Symbol::create_base(source_type, ticker, &market);
        let query = rlean_data::CustomDataQuery::from_properties(&properties);
        let config = rlean_data::CustomDataConfig {
            ticker: ticker.to_string(),
            source_type: source_type.to_string(),
            resolution,
            properties,
            query: query.clone(),
        };
        let metadata = CustomSubscriptionMetadata {
            source_type: source_type.to_string(),
            ticker: ticker.to_string(),
            config,
            dynamic_query: query,
        };
        self.subscription_manager
            .add(SubscriptionDataConfig::new_custom(
                symbol.clone(),
                resolution,
                metadata,
            ));
        self.ensure_base_security(symbol, resolution)
    }

    pub fn add_custom_universe_data(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
    ) -> Symbol {
        let market = Market::usa();
        let symbol = Symbol::create_base(source_type, ticker, &market);
        let query = rlean_data::CustomDataQuery::from_properties(&properties);
        let config = rlean_data::CustomDataConfig {
            ticker: ticker.to_string(),
            source_type: source_type.to_string(),
            resolution,
            properties,
            query: query.clone(),
        };
        let metadata = CustomSubscriptionMetadata {
            source_type: source_type.to_string(),
            ticker: ticker.to_string(),
            config,
            dynamic_query: query,
        };
        self.subscription_manager
            .add(SubscriptionDataConfig::new_custom_universe(
                symbol.clone(),
                resolution,
                metadata,
            ));
        self.ensure_base_security(symbol, resolution)
    }

    /// Register the provider-wide point-in-time fundamental cross-section used
    /// by the daily `AddUniverse(selector)` API. This is an internal feed: the
    /// selected equity subscriptions are added separately by universe changes.
    pub fn add_fundamental_universe_data(
        &mut self,
        source_type: &str,
        resolution: Resolution,
    ) -> Symbol {
        let market = Market::usa();
        let symbol = Symbol::create_base("fundamental_universe", source_type, &market);
        self.subscription_manager
            .add(SubscriptionDataConfig::new_fundamental_universe(
                symbol.clone(),
                resolution,
                FundamentalUniverseSubscriptionMetadata {
                    source_type: source_type.to_string(),
                },
            ));
        self.ensure_base_security(symbol, resolution)
    }

    fn add_base_symbol(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        self.ensure_base_security(symbol, resolution)
    }

    fn ensure_base_security(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        if self.securities.contains(&symbol) {
            return symbol;
        }
        let hours = self.market_hours_database.exchange_hours(&symbol);
        let props = SymbolProperties::default();
        let security = crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
            self.portfolio.holdings_store(),
        );
        self.initialize_security_models(&security);
        self.securities.add(security);
        symbol
    }

    fn add_equity_subscriptions(
        &self,
        symbol: Symbol,
        resolution: Resolution,
        normalization_mode: DataNormalizationMode,
    ) {
        self.subscription_manager
            .add(SubscriptionDataConfig::new_equity(
                symbol.clone(),
                resolution,
                normalization_mode,
            ));
        if resolution != Resolution::Hour && resolution != Resolution::Daily {
            let mut quote_config =
                SubscriptionDataConfig::new_equity(symbol, resolution, normalization_mode);
            quote_config.set_tick_type(rlean_core::TickType::Quote);
            self.subscription_manager.add(quote_config);
        }
    }

    /// LEAN parity: `Security.SetDataNormalizationMode(mode)` mutates all
    /// subscription configs attached to the symbol in place.
    pub fn set_data_normalization_mode(
        &self,
        symbol: &Symbol,
        normalization_mode: DataNormalizationMode,
    ) -> usize {
        self.subscription_manager
            .set_normalization_mode(symbol, normalization_mode)
    }

    pub fn add_forex(&mut self, ticker: &str, resolution: Resolution) -> Symbol {
        let symbol = Symbol::create_forex(ticker);
        let config = SubscriptionDataConfig::new_forex(symbol.clone(), resolution);
        self.subscription_manager.add(config);
        let hours = self.market_hours_database.exchange_hours(&symbol);
        let props = SymbolProperties::default();
        let security = crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
            self.portfolio.holdings_store(),
        );
        self.initialize_security_models(&security);
        self.securities.add(security);
        symbol
    }

    pub fn add_crypto(&mut self, ticker: &str, market: &Market, resolution: Resolution) -> Symbol {
        let symbol = Symbol::create_crypto(ticker, market);
        let config = SubscriptionDataConfig::new_crypto(symbol.clone(), resolution);
        self.subscription_manager.add(config);
        let mut quote_config = SubscriptionDataConfig::new_crypto(symbol.clone(), resolution);
        quote_config.set_tick_type(rlean_core::TickType::Quote);
        self.subscription_manager.add(quote_config);
        let hours = self.market_hours_database.exchange_hours(&symbol);
        let props = self.symbol_properties_for_symbol(&symbol);
        let security = crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
            self.portfolio.holdings_store(),
        );
        self.initialize_security_models(&security);
        self.securities.add(security);
        symbol
    }

    pub fn add_crypto_future(
        &mut self,
        ticker: &str,
        market: &Market,
        resolution: Resolution,
    ) -> Symbol {
        let symbol = Symbol::create_crypto_future(ticker, market);
        let trade_config = SubscriptionDataConfig::new_crypto_future(symbol.clone(), resolution);
        self.subscription_manager.add(trade_config);
        let mut quote_config =
            SubscriptionDataConfig::new_crypto_future(symbol.clone(), resolution);
        quote_config.set_tick_type(rlean_core::TickType::Quote);
        self.subscription_manager.add(quote_config);

        if self.securities.contains(&symbol) {
            return symbol;
        }

        let hours = self.market_hours_database.exchange_hours(&symbol);
        let props = self.symbol_properties_for_symbol(&symbol);
        let security = crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
            self.portfolio.holdings_store(),
        );
        self.initialize_security_models(&security);
        self.securities.add(security);
        symbol
    }

    fn symbol_properties_for_symbol(&self, symbol: &Symbol) -> SymbolProperties {
        let mut props = SymbolProperties::default();
        props.market_ticker = symbol.value.to_string();
        props.quote_currency = match symbol.security_type() {
            SecurityType::CryptoFuture if symbol.market().as_str() == Market::HYPERLIQUID => {
                "USDC".into()
            }
            SecurityType::Crypto | SecurityType::CryptoFuture => {
                infer_quote_currency(&symbol.value).unwrap_or_else(|| "USD".into())
            }
            _ => props.quote_currency,
        };
        props
    }

    pub fn contract_multiplier_for_symbol(&self, symbol: &Symbol) -> Decimal {
        self.securities
            .get(symbol)
            .and_then(|sec| Decimal::from_f64_retain(sec.symbol_properties.contract_multiplier))
            .unwrap_or_else(|| {
                if symbol.option_symbol_id().is_some() {
                    dec!(100)
                } else {
                    Decimal::ONE
                }
            })
    }

    pub fn order_fee(&self, order: &Order, fill_price: Price) -> OrderFee {
        let symbol = &order.symbol;
        let security_type = symbol.security_type();
        let (quote_currency, contract_multiplier) = self
            .securities
            .get(symbol)
            .map(|security| {
                (
                    security.symbol_properties.quote_currency.clone(),
                    Decimal::from_f64_retain(security.symbol_properties.contract_multiplier)
                        .unwrap_or(Decimal::ONE),
                )
            })
            .unwrap_or_else(|| {
                let props = self.symbol_properties_for_symbol(symbol);
                (
                    props.quote_currency,
                    self.contract_multiplier_for_symbol(symbol),
                )
            });
        let params = OrderFeeParameters {
            order,
            security_price: fill_price,
            security_type,
            quote_currency: quote_currency.clone(),
            base_currency: infer_base_currency(symbol, &quote_currency),
            contract_multiplier,
        };
        let model = self.fee_model_for_symbol(symbol);
        model.get_order_fee(&params)
    }

    fn fee_model_for_symbol(&self, symbol: &Symbol) -> Box<dyn FeeModel> {
        match (
            self.brokerage_name,
            symbol.security_type(),
            symbol.market().as_str(),
        ) {
            (BrokerageName::HyperliquidBrokerage, SecurityType::CryptoFuture, _)
            | (_, SecurityType::CryptoFuture, Market::HYPERLIQUID) => {
                Box::new(HyperliquidFeeModel::default())
            }
            (_, SecurityType::CryptoFuture, Market::BYBIT) => Box::new(BybitFeeModel::perpetuals()),
            (
                BrokerageName::Default,
                SecurityType::Equity
                | SecurityType::Option
                | SecurityType::Future
                | SecurityType::FutureOption,
                _,
            ) => Box::new(InteractiveBrokersFeeModel::default()),
            (BrokerageName::Default, _, _) => Box::new(FlatFeeModel::new(dec!(0))),
            (BrokerageName::TradierBrokerage, _, _) => Box::new(TradierFeeModel),
            (BrokerageName::InteractiveBrokersBrokerage, _, _) => {
                Box::new(InteractiveBrokersFeeModel::default())
            }
            _ => Box::new(InteractiveBrokersFeeModel::default()),
        }
    }

    fn security_contract_multiplier(&self, symbol: &Symbol) -> Decimal {
        self.securities
            .get(symbol)
            .and_then(|security| {
                Decimal::from_f64_retain(security.symbol_properties.contract_multiplier)
            })
            .unwrap_or_else(|| self.contract_multiplier_for_symbol(symbol))
    }

    fn quote_currency_for_symbol(&self, symbol: &Symbol) -> String {
        self.securities
            .get(symbol)
            .map(|security| security.symbol_properties.quote_currency.clone())
            .unwrap_or_else(|| self.symbol_properties_for_symbol(symbol).quote_currency)
    }

    fn shares_buying_power_pool(&self, left: &Symbol, right: &Symbol) -> bool {
        if left.security_type() == SecurityType::CryptoFuture
            || right.security_type() == SecurityType::CryptoFuture
        {
            return left.security_type() == SecurityType::CryptoFuture
                && right.security_type() == SecurityType::CryptoFuture
                && self
                    .quote_currency_for_symbol(left)
                    .eq_ignore_ascii_case(&self.quote_currency_for_symbol(right));
        }
        true
    }

    fn margin_used_after_order(
        &self,
        order: Option<(&Order, Price)>,
        scope_symbol: &Symbol,
    ) -> Price {
        let mut total = Decimal::ZERO;
        let mut order_symbol_in_holdings = false;
        for mut holding in self.portfolio.all_holdings() {
            if !self.shares_buying_power_pool(scope_symbol, &holding.symbol) {
                continue;
            }
            if let Some((order, fill_price)) = order {
                if holding.symbol.id.sid == order.symbol.id.sid {
                    holding.quantity += order.remaining_quantity();
                    holding.update_price(fill_price);
                    order_symbol_in_holdings = true;
                }
            }
            let Some(security) = self.securities.get(&holding.symbol) else {
                continue;
            };
            let price = if let Some((order, fill_price)) = order {
                if holding.symbol.id.sid == order.symbol.id.sid {
                    fill_price
                } else {
                    security.current_price()
                }
            } else {
                security.current_price()
            };
            total += security
                .buying_power_model()
                .maintenance_margin_requirement(
                    holding.quantity,
                    price,
                    holding.contract_multiplier,
                    security.leverage(),
                );
        }

        if let Some((order, fill_price)) = order {
            if !order_symbol_in_holdings
                && self.shares_buying_power_pool(scope_symbol, &order.symbol)
            {
                if let Some(security) = self.securities.get(&order.symbol) {
                    total += security
                        .buying_power_model()
                        .maintenance_margin_requirement(
                            order.remaining_quantity(),
                            fill_price,
                            self.security_contract_multiplier(&order.symbol),
                            security.leverage(),
                        );
                }
            }
        }
        total
    }

    pub fn total_margin_used(&self) -> Price {
        crate::margin_call::PositionGroupCollection::from_holdings(
            &self.portfolio.all_holdings(),
            &self.securities,
        )
        .total_reserved_buying_power()
    }

    pub fn margin_remaining(&self) -> Price {
        self.portfolio
            .margin_remaining_with_used(self.total_margin_used())
    }

    pub fn margin_remaining_for_symbol(&self, symbol: &Symbol) -> Price {
        let collateral = if symbol.security_type() == SecurityType::CryptoFuture {
            *self.portfolio.cash.read()
        } else {
            self.portfolio_value()
        };
        let used = self.margin_used_after_order(None, symbol);
        (collateral - used).max(Decimal::ZERO)
    }

    pub fn validate_order_buying_power(
        &self,
        order: &Order,
        fill_price: Price,
        order_fee: Price,
    ) -> Result<(), String> {
        if order.remaining_quantity().is_zero() {
            return Ok(());
        }
        let Some(security) = self.securities.get(&order.symbol) else {
            return Err(format!(
                "Insufficient buying power: security {} is not initialized",
                order.symbol.value
            ));
        };
        if fill_price <= Decimal::ZERO {
            return Err(format!(
                "Insufficient buying power: {} has no positive fill price",
                order.symbol.value
            ));
        }

        let collateral = if order.symbol.security_type() == SecurityType::CryptoFuture {
            *self.portfolio.cash.read()
        } else {
            self.portfolio_value()
        };
        let collateral_after_fee = (collateral - order_fee.max(Decimal::ZERO)).max(Decimal::ZERO);
        let used_after = self.margin_used_after_order(Some((order, fill_price)), &order.symbol);
        if used_after > collateral_after_fee {
            let leverage = security.leverage();
            return Err(format!(
                "Insufficient buying power for order {} {} {} @ {}. Margin required after order: {}, collateral available after fees: {}, leverage: {}",
                order.id,
                order.symbol.value,
                order.remaining_quantity(),
                fill_price,
                used_after,
                collateral_after_fee,
                leverage
            ));
        }
        Ok(())
    }

    // ─── Ordering ────────────────────────────────────────────────────────────

    /// Allocate the next engine order id from the algorithm's single order-id
    /// authority. Framework/algorithm orders and any engine-initiated orders
    /// (e.g. startup liquidations) must all draw ids from this one counter so no
    /// two live orders ever share an id. Public so the live runner can allocate
    /// liquidation-order ids from the same sequence framework orders use.
    pub fn next_order_id(&mut self) -> i64 {
        self.order_id_counter += 1;
        self.order_id_counter
    }

    fn submit_order(&self, mut order: Order) -> OrderTicket {
        self.apply_order_submission_data(&mut order);

        if let Some(message) = self.validate_brokerage_order(&order) {
            let event = OrderEvent::invalid(order.id, order.symbol.clone(), self.utc_time, message);
            order.status = OrderStatus::Invalid;
            let ticket = self.transactions.add_order(order);
            self.transactions.process_order_event(event);
            return ticket;
        }

        self.transactions.add_order(order)
    }

    fn apply_order_submission_data(&self, order: &mut Order) {
        if order.order_submission_data.is_some() {
            return;
        }

        if let Some(security) = self.securities.get(&order.symbol) {
            let last = security.current_price();
            let bid = {
                let bid = security.bid_price();
                if bid > Decimal::ZERO {
                    bid
                } else {
                    last
                }
            };
            let ask = {
                let ask = security.ask_price();
                if ask > Decimal::ZERO {
                    ask
                } else {
                    last
                }
            };
            order.order_submission_data = Some(OrderSubmissionData::new(bid, ask, last));
        }
    }

    fn validate_brokerage_order(&self, order: &Order) -> Option<String> {
        if self.hyperliquid_post_only_order_crosses_book(order) {
            return Some(
                "Hyperliquid post-only limit orders must not cross the current bid/ask".into(),
            );
        }

        if self.brokerage_name != BrokerageName::TradierBrokerage {
            return None;
        }

        if !matches!(
            order.order_type,
            OrderType::Limit | OrderType::Market | OrderType::StopMarket | OrderType::StopLimit
        ) {
            return Some(format!(
                "Tradier does not support {:?} orders",
                order.order_type
            ));
        }

        if !matches!(
            order.symbol.security_type(),
            SecurityType::Equity | SecurityType::Option | SecurityType::IndexOption
        ) {
            return Some(format!(
                "Tradier does not support {:?} securities",
                order.symbol.security_type()
            ));
        }

        if !matches!(
            order.time_in_force,
            TimeInForce::GoodTilCanceled | TimeInForce::Day
        ) {
            return Some(format!(
                "Tradier does not support {:?} time in force",
                order.time_in_force
            ));
        }

        let absolute_quantity = order.abs_quantity();
        if absolute_quantity < dec!(1) || absolute_quantity > dec!(10000000) {
            return Some(format!(
                "Tradier order quantity must be between 1 and 10000000, got {}",
                absolute_quantity
            ));
        }

        let holding_quantity = self.portfolio.get_holding(&order.symbol).quantity;
        let projected_quantity = holding_quantity + order.quantity;
        if projected_quantity < dec!(0) {
            if order.time_in_force == TimeInForce::GoodTilCanceled {
                return Some(
                    "Tradier does not support GTC orders that leave a short position".into(),
                );
            }

            let security_price = self
                .securities
                .get(&order.symbol)
                .map(|security| security.current_price())
                .unwrap_or(order.price);
            if security_price < dec!(5) {
                return Some(
                    "Tradier does not support short sale orders for securities priced below $5"
                        .into(),
                );
            }
        }

        if order.properties.outside_regular_trading_hours
            && !self.tradier_can_execute_order_at(order, order.time)
        {
            return Some(
                "Tradier extended-hours orders must be equity limit orders submitted during the current pre-market or post-market session"
                    .into(),
            );
        }

        None
    }

    fn hyperliquid_post_only_order_crosses_book(&self, order: &Order) -> bool {
        if self.brokerage_name != BrokerageName::HyperliquidBrokerage
            && !matches!(
                (order.symbol.security_type(), order.symbol.market().as_str()),
                (SecurityType::CryptoFuture, Market::HYPERLIQUID)
            )
        {
            return false;
        }
        if order.order_type != OrderType::Limit || !order.properties.post_only {
            return false;
        }
        let Some(limit_price) = order.limit_price else {
            return false;
        };
        let Some(security) = self.securities.get(&order.symbol) else {
            return false;
        };
        let bid = security.bid_price();
        let ask = security.ask_price();

        if order.quantity > Decimal::ZERO {
            ask > Decimal::ZERO && limit_price >= ask
        } else if order.quantity < Decimal::ZERO {
            bid > Decimal::ZERO && limit_price <= bid
        } else {
            false
        }
    }

    pub fn can_execute_order_with_brokerage_model(&self, order: &Order) -> bool {
        if self.brokerage_name != BrokerageName::TradierBrokerage {
            return true;
        }

        self.tradier_can_execute_order_at(order, self.utc_time)
    }

    fn tradier_can_execute_order_at(&self, order: &Order, time: DateTime) -> bool {
        let Some(security) = self.securities.get(&order.symbol) else {
            return true;
        };

        if security.exchange_hours.is_open_at(time) {
            return true;
        }

        if !order.properties.outside_regular_trading_hours {
            return false;
        }

        order.order_type == OrderType::Limit
            && order.symbol.security_type() == SecurityType::Equity
            && tradier_extended_session(time).is_some()
    }

    pub fn market_order(&mut self, symbol: &Symbol, quantity: Quantity) -> OrderTicket {
        self.market_order_with_time_in_force(symbol, quantity, None)
    }

    pub fn market_order_with_time_in_force(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        time_in_force: Option<TimeInForce>,
    ) -> OrderTicket {
        self.market_order_with_options(symbol, quantity, time_in_force, false)
    }

    pub fn market_order_with_options(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicket {
        self.market_order_with_options_and_tag(
            symbol,
            quantity,
            time_in_force,
            outside_regular_trading_hours,
            "",
        )
    }

    pub fn market_order_with_options_and_tag(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
        tag: &str,
    ) -> OrderTicket {
        if symbol.option_symbol_id().is_some() {
            self.ensure_option_security(symbol, Resolution::Minute);
        }
        let id = self.next_order_id();
        let mut order = Order::market(id, symbol.clone(), quantity, self.utc_time, tag);
        if let Some(time_in_force) = time_in_force {
            order.time_in_force = time_in_force;
        }
        order.properties.outside_regular_trading_hours = outside_regular_trading_hours;
        self.submit_order(order)
    }

    pub fn limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
    ) -> OrderTicket {
        self.limit_order_with_time_in_force(symbol, quantity, limit_price, None)
    }

    pub fn limit_order_with_time_in_force(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
        time_in_force: Option<TimeInForce>,
    ) -> OrderTicket {
        self.limit_order_with_options(symbol, quantity, limit_price, time_in_force, false)
    }

    pub fn limit_order_with_options(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicket {
        self.limit_order_with_properties(
            symbol,
            quantity,
            limit_price,
            time_in_force,
            outside_regular_trading_hours,
            false,
        )
    }

    pub fn limit_order_with_properties(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
        post_only: bool,
    ) -> OrderTicket {
        self.limit_order_with_properties_and_tag(
            symbol,
            quantity,
            limit_price,
            time_in_force,
            outside_regular_trading_hours,
            post_only,
            "",
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn limit_order_with_properties_and_tag(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
        post_only: bool,
        tag: &str,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let mut order = Order::limit(
            id,
            symbol.clone(),
            quantity,
            limit_price,
            self.utc_time,
            tag,
        );
        if let Some(time_in_force) = time_in_force {
            order.time_in_force = time_in_force;
        }
        order.properties.outside_regular_trading_hours = outside_regular_trading_hours;
        order.properties.post_only = post_only;
        self.submit_order(order)
    }

    pub fn stop_market_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
    ) -> OrderTicket {
        self.stop_market_order_with_time_in_force(symbol, quantity, stop_price, None)
    }

    pub fn stop_market_order_with_time_in_force(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
        time_in_force: Option<TimeInForce>,
    ) -> OrderTicket {
        self.stop_market_order_with_options(symbol, quantity, stop_price, time_in_force, false)
    }

    pub fn stop_market_order_with_options(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let mut order =
            Order::stop_market(id, symbol.clone(), quantity, stop_price, self.utc_time, "");
        if let Some(time_in_force) = time_in_force {
            order.time_in_force = time_in_force;
        }
        order.properties.outside_regular_trading_hours = outside_regular_trading_hours;
        self.submit_order(order)
    }

    pub fn stop_limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
        limit_price: Price,
    ) -> OrderTicket {
        self.stop_limit_order_with_time_in_force(symbol, quantity, stop_price, limit_price, None)
    }

    pub fn stop_limit_order_with_time_in_force(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
        limit_price: Price,
        time_in_force: Option<TimeInForce>,
    ) -> OrderTicket {
        self.stop_limit_order_with_options(
            symbol,
            quantity,
            stop_price,
            limit_price,
            time_in_force,
            false,
        )
    }

    pub fn stop_limit_order_with_options(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
        limit_price: Price,
        time_in_force: Option<TimeInForce>,
        outside_regular_trading_hours: bool,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let mut order = Order::stop_limit(
            id,
            symbol.clone(),
            quantity,
            stop_price,
            limit_price,
            self.utc_time,
            "",
        );
        if let Some(time_in_force) = time_in_force {
            order.time_in_force = time_in_force;
        }
        order.properties.outside_regular_trading_hours = outside_regular_trading_hours;
        self.submit_order(order)
    }

    /// Market-on-open order.
    pub fn market_on_open_order(&mut self, symbol: &Symbol, quantity: Quantity) -> OrderTicket {
        let id = self.next_order_id();
        let mut order = Order::market(id, symbol.clone(), quantity, self.utc_time, "");
        order.order_type = OrderType::MarketOnOpen;
        self.submit_order(order)
    }

    /// Market-on-close order.
    pub fn market_on_close_order(&mut self, symbol: &Symbol, quantity: Quantity) -> OrderTicket {
        let id = self.next_order_id();
        let mut order = Order::market(id, symbol.clone(), quantity, self.utc_time, "");
        order.order_type = OrderType::MarketOnClose;
        self.submit_order(order)
    }

    /// Trailing stop order.
    ///
    /// `trailing_amount` is either a percentage (e.g. `dec!(0.05)` for 5%) when
    /// `trailing_as_percentage` is `true`, or an absolute dollar amount otherwise.
    /// `stop_price` is the initial stop — pass `Decimal::ZERO` to have it
    /// computed automatically on the first price update.
    pub fn trailing_stop_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        trailing_amount: Price,
        trailing_as_percentage: bool,
        stop_price: Price,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let tso = TrailingStopOrder::new(
            id,
            symbol.clone(),
            quantity,
            TrailingStopOrderParams {
                trailing_amount,
                trailing_as_percentage,
                stop_price,
                time: self.utc_time,
                tag: "",
            },
        );
        self.submit_order(tso.order)
    }

    /// Limit-if-touched order.
    ///
    /// Once `trigger_price` is touched, a limit order at `limit_price` is activated.
    pub fn limit_if_touched(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        trigger_price: Price,
        limit_price: Price,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let lit = LimitIfTouchedOrder::new(
            id,
            symbol.clone(),
            quantity,
            trigger_price,
            limit_price,
            self.utc_time,
            "",
        );
        self.submit_order(lit.order)
    }

    /// Combo market order — all legs execute simultaneously at market prices.
    ///
    /// `symbol` and `quantity` describe the primary leg; `legs` describes all
    /// legs in the group (typically includes the primary leg as well).
    pub fn combo_market_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        legs: Vec<ComboLegDetails>,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let cmo = ComboMarketOrder::new(id, symbol.clone(), quantity, self.utc_time, "", legs);
        self.submit_order(cmo.order)
    }

    /// Combo limit order — all legs execute as a unit at a net `limit_price`.
    pub fn combo_limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
        legs: Vec<ComboLegDetails>,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let clo = ComboLimitOrder::new(
            id,
            symbol.clone(),
            quantity,
            limit_price,
            self.utc_time,
            "",
            legs,
        );
        self.submit_order(clo.order)
    }

    /// Combo leg limit order — each leg has its own per-leg limit price.
    pub fn combo_leg_limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
        legs: Vec<ComboLegDetails>,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let cll = ComboLegLimitOrder::new(
            id,
            symbol.clone(),
            quantity,
            limit_price,
            self.utc_time,
            "",
            legs,
        );
        self.submit_order(cll.order)
    }

    /// Set holdings to a target portfolio weight (0.0 = 0%, 1.0 = 100% of portfolio value).
    pub fn set_holdings(&mut self, symbol: &Symbol, target: Decimal) -> Option<OrderTicket> {
        let portfolio_value = self.portfolio_value();
        let security = self.securities.get(symbol)?;
        let current_price = security.current_price();

        if current_price.is_zero() {
            return None;
        }

        let current_holding = self.portfolio.get_holding(symbol);
        let contract_multiplier = self.security_contract_multiplier(symbol);
        let leverage = security.leverage();
        let target_margin_percent = target / BuyingPowerModel::leverage_decimal(leverage);
        let target_quantity = security
            .buying_power_model()
            .target_quantity_for_buying_power(
                portfolio_value,
                target_margin_percent,
                current_price,
                contract_multiplier,
                leverage,
            );
        let delta_quantity = target_quantity - current_holding.quantity;
        let delta_margin = security.buying_power_model().initial_margin_requirement(
            delta_quantity,
            current_price,
            contract_multiplier,
            leverage,
        );

        if delta_margin.abs() < dec!(1) {
            return None;
        } // avoid tiny orders

        // Truncate (floor toward zero) to integer, matching C# LEAN's lot-size behavior
        let qty_rounded = delta_quantity.trunc();

        if qty_rounded.is_zero() {
            return None;
        }

        Some(self.market_order(symbol, qty_rounded))
    }

    /// Liquidate all holdings in a symbol.
    pub fn liquidate(&mut self, symbol: Option<&Symbol>) -> Vec<OrderTicket> {
        let symbols = match symbol {
            Some(s) => vec![s.clone()],
            None => self.portfolio.invested_symbols(),
        };

        let mut tickets = Vec::new();
        for sym in symbols {
            let holding = self.portfolio.get_holding(&sym);
            if holding.is_invested() {
                let ticket = self.market_order(&sym, -holding.quantity);
                tickets.push(ticket);
            }
        }
        tickets
    }

    // ─── Indicators ──────────────────────────────────────────────────────────

    pub fn sma(&self, period: usize) -> rlean_indicators::Sma {
        rlean_indicators::Sma::new(period)
    }

    pub fn ema(&self, period: usize) -> rlean_indicators::Ema {
        rlean_indicators::Ema::new(period)
    }

    pub fn rsi(&self, period: usize) -> rlean_indicators::Rsi {
        rlean_indicators::Rsi::new(period)
    }

    pub fn macd(&self, fast: usize, slow: usize, signal: usize) -> rlean_indicators::Macd {
        rlean_indicators::Macd::new(fast, slow, signal)
    }

    pub fn bb(&self, period: usize, k: Decimal) -> rlean_indicators::BollingerBands {
        rlean_indicators::BollingerBands::new(period, k)
    }

    pub fn atr(&self, period: usize) -> rlean_indicators::Atr {
        rlean_indicators::Atr::new(period)
    }

    pub fn adx(&self, period: usize) -> rlean_indicators::Adx {
        rlean_indicators::Adx::new(period)
    }

    pub fn stochastic(&self, k_period: usize, d_period: usize) -> rlean_indicators::Stochastic {
        rlean_indicators::Stochastic::new(k_period, d_period)
    }

    pub fn roc(&self, period: usize) -> rlean_indicators::Roc {
        rlean_indicators::Roc::new(period)
    }

    pub fn cci(&self, period: usize) -> rlean_indicators::Cci {
        rlean_indicators::Cci::new(period)
    }

    pub fn donchian(&self, period: usize) -> rlean_indicators::DonchianChannel {
        rlean_indicators::DonchianChannel::new(period)
    }

    pub fn keltner(&self, period: usize, multiplier: Decimal) -> rlean_indicators::KeltnerChannel {
        rlean_indicators::KeltnerChannel::new(period, multiplier)
    }

    pub fn vwap(&self) -> rlean_indicators::Vwap {
        rlean_indicators::Vwap::new()
    }

    pub fn obv(&self) -> rlean_indicators::Obv {
        rlean_indicators::Obv::new()
    }

    // ─── Logging ─────────────────────────────────────────────────────────────

    pub fn debug(&self, message: impl Into<String>) {
        self.log.debug(self.utc_time, message);
    }

    pub fn log_message(&self, message: impl Into<String>) {
        self.log.info(self.utc_time, message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.log.error(self.utc_time, message);
    }

    // ─── Portfolio Helpers ───────────────────────────────────────────────────

    pub fn cash(&self) -> Price {
        *self.portfolio.cash.read()
    }
    pub fn portfolio_value(&self) -> Price {
        let cash = *self.portfolio.cash.read();
        let holdings_value: Price = self
            .portfolio
            .all_holdings()
            .into_iter()
            .filter(|holding| holding.is_invested())
            .map(|holding| {
                let price = self
                    .securities
                    .get(&holding.symbol)
                    .map(|security| security.current_price())
                    .filter(|price| !price.is_zero())
                    .unwrap_or(holding.last_price);
                match holding.symbol.security_type() {
                    SecurityType::CryptoFuture => {
                        (price - holding.average_price)
                            * holding.quantity
                            * holding.contract_multiplier
                    }
                    _ => holding.get_quantity_value(holding.quantity, price),
                }
            })
            .sum();
        cash + holdings_value
    }

    pub fn unrealized_profit(&self) -> Price {
        self.portfolio
            .all_holdings()
            .into_iter()
            .filter(|holding| holding.is_invested())
            .map(|holding| {
                let price = self
                    .securities
                    .get(&holding.symbol)
                    .map(|security| security.current_price())
                    .filter(|price| !price.is_zero())
                    .unwrap_or(holding.last_price);
                (price - holding.average_price) * holding.quantity * holding.contract_multiplier
            })
            .sum()
    }

    pub fn total_holdings_value(&self) -> Price {
        self.portfolio
            .all_holdings()
            .into_iter()
            .filter(|holding| holding.is_invested())
            .map(|holding| {
                let price = self
                    .securities
                    .get(&holding.symbol)
                    .map(|security| security.current_price())
                    .filter(|price| !price.is_zero())
                    .unwrap_or(holding.last_price);
                holding.get_quantity_value(holding.quantity, price).abs()
            })
            .sum()
    }
    pub fn is_invested(&self, symbol: &Symbol) -> bool {
        self.portfolio.is_invested(symbol)
    }

    pub fn is_option_underlying(&self, symbol: &Symbol) -> bool {
        self.option_subscriptions.iter().any(|canonical| {
            canonical
                .underlying
                .as_ref()
                .map(|underlying| underlying.id.sid == symbol.id.sid)
                .unwrap_or_else(|| {
                    canonical
                        .permtick
                        .trim_start_matches('?')
                        .eq_ignore_ascii_case(&symbol.permtick)
                })
        })
    }

    // ─── Options ─────────────────────────────────────────────────────────────

    /// Subscribe to the option chain for an underlying equity.
    /// Returns a canonical option Symbol (e.g., `?SPY`) that can be used
    /// to access the option chain in `on_data()`.
    pub fn add_option(&mut self, underlying_ticker: &str, resolution: Resolution) -> Symbol {
        let underlying = self.add_equity(underlying_ticker, resolution);
        // C# Lean forces the underlying equity to Raw when an option universe
        // (or contract) is subscribed — see `OptionChainUniverse` and
        // `QCAlgorithm.AddOptionContract`.
        self.subscription_manager
            .set_normalization_mode(&underlying, DataNormalizationMode::Raw);
        let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
        if !self
            .option_subscriptions
            .iter()
            .any(|symbol| symbol.id.sid == canonical.id.sid)
        {
            self.option_subscriptions.push(canonical.clone());
        }
        self.option_subscription_resolutions
            .insert(canonical.permtick.to_string(), resolution);
        self.option_filters
            .insert(canonical.permtick.to_string(), OptionFilter::default());
        self.option_subscriptions_generation += 1;
        canonical
    }

    pub fn set_option_filter(&mut self, canonical: &Symbol, filter: OptionFilter) {
        self.option_filters
            .insert(canonical.permtick.to_string(), filter);
        self.option_subscriptions_generation += 1;
    }

    /// Subscribe to a specific option contract.
    pub fn add_option_contract(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        // Add the underlying equity subscription if not already tracked.
        if let Some(ref u) = symbol.underlying {
            if !self.securities.contains(u) {
                self.add_equity(&u.permtick, resolution);
            }
            // C# Lean's `AddOptionContract` forces the underlying configs to Raw
            // (see `QCAlgorithm.AddOptionContract`).
            self.subscription_manager
                .set_normalization_mode(u, DataNormalizationMode::Raw);
        }
        self.ensure_option_security(&symbol, resolution);
        if !self
            .open_option_contracts
            .iter()
            .any(|existing| existing.id.sid == symbol.id.sid)
        {
            self.open_option_contracts.push(symbol.clone());
        }
        symbol
    }

    /// Remove a security subscription, matching LEAN's `RemoveSecurity` surface.
    ///
    /// For canonical options, this removes the option universe/filter/chain and
    /// any unheld child option contract securities. Underlying equities are not
    /// removed here because rlean does not yet mark add-option underlyings as
    /// internal feeds, while user strategies often subscribe to the same equity
    /// through an explicit universe.
    pub fn remove_security(&mut self, symbol: &Symbol, _tag: Option<&str>) -> bool {
        if symbol.is_canonical_option() {
            return self.remove_option_subscription(symbol);
        }

        let existed = self.securities.contains(symbol)
            || self
                .open_option_contracts
                .iter()
                .any(|existing| existing.id.sid == symbol.id.sid);
        if !existed {
            return false;
        }

        self.transactions
            .cancel_open_orders_for_symbol(symbol.id.sid, self.utc_time);
        if self.is_invested(symbol) {
            self.liquidate(Some(symbol));
        }
        self.subscription_manager.remove_symbol(symbol);
        self.securities.remove(symbol);
        self.open_option_contracts
            .retain(|existing| existing.id.sid != symbol.id.sid);
        true
    }

    /// LEAN sugar for `RemoveSecurity` on a specific option contract.
    pub fn remove_option_contract(&mut self, symbol: &Symbol, tag: Option<&str>) -> bool {
        self.remove_security(symbol, tag)
    }

    fn remove_option_subscription(&mut self, canonical: &Symbol) -> bool {
        let canonical_key = canonical.permtick.to_string();
        let underlying_key = Self::canonical_underlying_key(canonical);
        let existed = self
            .option_subscriptions
            .iter()
            .any(|symbol| symbol.id.sid == canonical.id.sid);
        if !existed {
            return false;
        }

        self.option_subscriptions
            .retain(|symbol| symbol.id.sid != canonical.id.sid);
        self.option_subscription_resolutions.remove(&canonical_key);
        self.option_filters.remove(&canonical_key);
        self.option_chains.remove(&canonical_key);
        self.option_subscriptions_generation += 1;

        let child_symbols: Vec<Symbol> = self
            .open_option_contracts
            .iter()
            .filter(|symbol| Self::option_symbol_matches_underlying(symbol, &underlying_key))
            .cloned()
            .collect();
        for child in child_symbols {
            self.transactions
                .cancel_open_orders_for_symbol(child.id.sid, self.utc_time);
            if self.is_invested(&child) {
                self.liquidate(Some(&child));
            }
            self.subscription_manager.remove_symbol(&child);
            self.securities.remove(&child);
            self.open_option_contracts
                .retain(|existing| existing.id.sid != child.id.sid);
        }

        true
    }

    fn canonical_underlying_key(canonical: &Symbol) -> String {
        canonical
            .underlying
            .as_ref()
            .map(|underlying| underlying.permtick.to_ascii_uppercase())
            .unwrap_or_else(|| {
                canonical
                    .permtick
                    .trim_start_matches('?')
                    .to_ascii_uppercase()
            })
    }

    fn option_symbol_matches_underlying(symbol: &Symbol, underlying_key: &str) -> bool {
        symbol
            .underlying
            .as_ref()
            .map(|underlying| underlying.permtick.eq_ignore_ascii_case(underlying_key))
            .unwrap_or(false)
    }

    pub fn ensure_option_security(&mut self, symbol: &Symbol, resolution: Resolution) {
        self.add_option_contract_subscriptions(symbol.clone(), resolution);
        if self.securities.contains(symbol) {
            return;
        }
        let hours = self.market_hours_database.exchange_hours(symbol);
        let props = SymbolProperties {
            contract_multiplier: 100.0,
            ..SymbolProperties::default()
        };
        let security = crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
            self.portfolio.holdings_store(),
        );
        self.initialize_security_models(&security);
        self.securities.add(security);
    }

    fn add_option_contract_subscriptions(&self, symbol: Symbol, resolution: Resolution) {
        if symbol.is_canonical_option() {
            return;
        }

        // Options are always Raw — `new_option` enforces that.
        self.subscription_manager
            .add(SubscriptionDataConfig::new_option(
                symbol.clone(),
                resolution,
            ));
        if resolution != Resolution::Hour && resolution != Resolution::Daily {
            let mut quote_config = SubscriptionDataConfig::new_option(symbol, resolution);
            quote_config.set_tick_type(rlean_core::TickType::Quote);
            self.subscription_manager.add(quote_config);
        }
    }

    fn option_contract_multiplier(&self, symbol: &Symbol) -> Decimal {
        self.securities
            .get(symbol)
            .and_then(|sec| Decimal::from_f64_retain(sec.symbol_properties.contract_multiplier))
            .unwrap_or(dec!(100))
    }

    /// Sell to open: short an option contract (collect premium).
    /// Credits the total premium to cash and records the short position.
    /// Returns a synthetic order ID.
    pub fn sell_to_open(
        &mut self,
        symbol: Symbol,
        quantity: Decimal,
        premium_per_contract: Decimal,
    ) -> i64 {
        self.ensure_option_security(&symbol, Resolution::Minute);
        let multiplier = self.option_contract_multiplier(&symbol);
        let total_premium = premium_per_contract * quantity * multiplier;
        self.portfolio.apply_fill_with_multiplier(
            &symbol,
            premium_per_contract,
            -quantity,
            dec!(0),
            multiplier,
        );

        let order_id = self.next_order_id();
        tracing::info!(
            "SELL TO OPEN {} x{} @ {} (premium: {})",
            symbol.value,
            quantity,
            premium_per_contract,
            total_premium
        );
        order_id
    }

    /// Buy to open: long an option contract (pay premium).
    /// Debits the total cost from cash and records the long position.
    /// Returns a synthetic order ID.
    pub fn buy_to_open(
        &mut self,
        symbol: Symbol,
        quantity: Decimal,
        premium_per_contract: Decimal,
    ) -> i64 {
        self.ensure_option_security(&symbol, Resolution::Minute);
        let multiplier = self.option_contract_multiplier(&symbol);
        let total_cost = premium_per_contract * quantity * multiplier;
        self.portfolio.apply_fill_with_multiplier(
            &symbol,
            premium_per_contract,
            quantity,
            dec!(0),
            multiplier,
        );

        let order_id = self.next_order_id();
        tracing::info!(
            "BUY TO OPEN {} x{} @ {} (cost: {})",
            symbol.value,
            quantity,
            premium_per_contract,
            total_cost
        );
        order_id
    }

    /// Buy to close: exit a short option position.
    /// Debits the close cost from cash and removes the tracked position.
    /// Returns a synthetic order ID.
    pub fn buy_to_close(
        &mut self,
        symbol: Symbol,
        quantity: Decimal,
        premium_per_contract: Decimal,
    ) -> i64 {
        self.ensure_option_security(&symbol, Resolution::Minute);
        let multiplier = self.option_contract_multiplier(&symbol);
        self.portfolio.apply_fill_with_multiplier(
            &symbol,
            premium_per_contract,
            quantity,
            dec!(0),
            multiplier,
        );
        let order_id = self.next_order_id();
        tracing::info!(
            "BUY TO CLOSE {} x{} @ {}",
            symbol.value,
            quantity,
            premium_per_contract
        );
        order_id
    }

    /// Sell to close: exit a long option position.
    /// Credits the sale proceeds to cash and removes the tracked position.
    /// Returns a synthetic order ID.
    pub fn sell_to_close(
        &mut self,
        symbol: Symbol,
        quantity: Decimal,
        premium_per_contract: Decimal,
    ) -> i64 {
        self.ensure_option_security(&symbol, Resolution::Minute);
        let multiplier = self.option_contract_multiplier(&symbol);
        self.portfolio.apply_fill_with_multiplier(
            &symbol,
            premium_per_contract,
            -quantity,
            dec!(0),
            multiplier,
        );
        let order_id = self.next_order_id();
        tracing::info!(
            "SELL TO CLOSE {} x{} @ {}",
            symbol.value,
            quantity,
            premium_per_contract
        );
        order_id
    }

    /// Returns all currently open option positions.
    pub fn get_option_positions(&self) -> Vec<OpenOptionPosition> {
        self.portfolio
            .all_holdings()
            .into_iter()
            .filter(|holding| holding.is_invested() && holding.symbol.option_symbol_id().is_some())
            .filter_map(|holding| {
                let option_id = holding.symbol.option_symbol_id()?;
                Some(OpenOptionPosition {
                    symbol: holding.symbol,
                    strike: option_id.strike,
                    expiry: option_id.expiry,
                    right: option_id.right,
                    style: option_id.style,
                    settlement: SettlementType::PhysicalDelivery,
                    quantity: holding.quantity,
                    entry_price: holding.average_price,
                    contract_unit_of_trade: holding
                        .contract_multiplier
                        .trunc()
                        .to_i64()
                        .unwrap_or(100),
                })
            })
            .collect()
    }

    /// Returns the most recently generated option chain for a canonical ticker.
    pub fn get_option_chain(&self, canonical: &str) -> Option<OptionChain> {
        self.option_chains.get(canonical).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal_macros::dec;

    #[test]
    fn default_brokerage_fee_model_matches_lean_security_type_defaults() {
        let time = DateTime::from_secs(0);

        let alg = QcAlgorithm::new("test", dec!(100_000));
        let equity = Symbol::create_equity("SPY", &Market::usa());
        let equity_order = Order::market(1, equity, dec!(471), time, "");
        let equity_fee = alg.order_fee(&equity_order, dec!(42.48));
        assert_eq!(equity_fee.amount, dec!(2.355));
        assert_eq!(equity_fee.currency, "USD");

        let crypto = Symbol::create_crypto("BTCUSD", &Market::coinbase());
        let crypto_order = Order::market(2, crypto, dec!(1), time, "");
        let crypto_fee = alg.order_fee(&crypto_order, dec!(50_000));
        assert_eq!(crypto_fee.amount, dec!(0));

        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let option = Symbol::create_option(
            underlying,
            &Market::usa(),
            NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
            dec!(500),
            OptionRight::Call,
            OptionStyle::American,
        );
        let option_order = Order::market(3, option, dec!(2), time, "");
        let option_fee = alg.order_fee(&option_order, dec!(3.25));
        assert_eq!(option_fee.amount, dec!(1.40));

        let future = Symbol::create_future(
            "ES",
            &Market::cme(),
            NaiveDate::from_ymd_opt(2026, 3, 20).unwrap(),
        );
        let future_order = Order::market(4, future, dec!(3), time, "");
        let future_fee = alg.order_fee(&future_order, dec!(5_000));
        assert_eq!(future_fee.amount, dec!(2.55));
    }

    #[test]
    fn tradier_equity_fee_model_remains_zero_fee() {
        let mut alg = QcAlgorithm::new("test", dec!(100_000));
        alg.set_brokerage_model(BrokerageName::TradierBrokerage, AccountType::Margin);

        let equity = Symbol::create_equity("SPY", &Market::usa());
        let order = Order::market(1, equity, dec!(471), DateTime::from_secs(0), "");
        let fee = alg.order_fee(&order, dec!(42.48));

        assert_eq!(fee.amount, dec!(0));
    }

    #[test]
    fn warmup_resolution_is_stored_and_cleared_with_warmup_state() {
        let mut alg = QcAlgorithm::new("test", dec!(100_000));

        alg.set_warm_up_bars_with_resolution(200, Some(Resolution::Daily));

        assert_eq!(alg.warmup_bar_count, Some(200));
        assert_eq!(alg.warmup_resolution, Some(Resolution::Daily));
        assert!(alg.is_warming_up);

        alg.end_warm_up();

        assert_eq!(alg.warmup_bar_count, None);
        assert_eq!(alg.warmup_resolution, None);
        assert!(!alg.is_warming_up);
    }

    #[test]
    fn portfolio_value_less_free_buffer_matches_lean_default() {
        let alg = QcAlgorithm::new("test", dec!(100_000));

        assert_eq!(alg.free_portfolio_value_percentage, dec!(0.0025));
        assert_eq!(alg.portfolio_value_less_free_buffer(), dec!(99_750));
        assert_eq!(alg.minimum_order_margin_portfolio_percentage, dec!(0.001));
    }

    #[test]
    fn remove_security_removes_equity_subscription_and_security() {
        let mut alg = QcAlgorithm::new("test", dec!(100_000));
        let symbol = alg.add_equity("MSFT", Resolution::Minute);

        assert!(alg.securities.contains(&symbol));
        assert!(alg
            .subscription_manager
            .get_all()
            .iter()
            .any(|config| config.symbol.id.sid == symbol.id.sid));

        assert!(alg.remove_security(&symbol, None));
        assert!(!alg.securities.contains(&symbol));
        assert!(!alg
            .subscription_manager
            .get_all()
            .iter()
            .any(|config| config.symbol.id.sid == symbol.id.sid));
    }

    #[test]
    fn remove_security_removes_canonical_option_state_but_keeps_underlying() {
        let mut alg = QcAlgorithm::new("test", dec!(100_000));
        let canonical = alg.add_option("AAPL", Resolution::Minute);
        let underlying = canonical.underlying.as_ref().unwrap().as_ref().clone();
        let contract = Symbol::create_option(
            underlying.clone(),
            &Market::usa(),
            NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
            dec!(250),
            OptionRight::Call,
            OptionStyle::American,
        );

        alg.set_option_filter(
            &canonical,
            OptionFilter {
                min_strike_rank: -5,
                max_strike_rank: 25,
                min_expiry_days: 7,
                max_expiry_days: 45,
            },
        );
        alg.option_chains.insert(
            canonical.permtick.to_string(),
            OptionChain::new(canonical.clone(), dec!(200)),
        );
        alg.add_option_contract(contract.clone(), Resolution::Minute);

        assert!(alg
            .option_subscriptions
            .iter()
            .any(|symbol| symbol.id.sid == canonical.id.sid));
        assert!(alg.securities.contains(&underlying));
        assert!(alg.is_option_underlying(&underlying));
        assert!(alg.securities.contains(&contract));

        assert!(alg.remove_security(&canonical, None));

        assert!(!alg
            .option_subscriptions
            .iter()
            .any(|symbol| symbol.id.sid == canonical.id.sid));
        assert!(!alg
            .option_subscription_resolutions
            .contains_key(canonical.permtick.as_ref()));
        assert!(!alg.option_filters.contains_key(canonical.permtick.as_ref()));
        assert!(!alg.option_chains.contains_key(canonical.permtick.as_ref()));
        assert!(!alg.is_option_underlying(&underlying));
        assert!(!alg
            .open_option_contracts
            .iter()
            .any(|symbol| symbol.id.sid == contract.id.sid));
        assert!(!alg.securities.contains(&contract));
        assert!(alg.securities.contains(&underlying));
        assert!(alg
            .subscription_manager
            .get_all()
            .iter()
            .any(|config| config.symbol.id.sid == underlying.id.sid));
    }
}
