use crate::{
    algorithm::AlgorithmStatus, logging::AlgorithmLogging, portfolio::SecurityPortfolioManager,
    runtime_statistics::RuntimeStatistics, securities::SecurityManager,
};
use lean_core::exchange_hours::ExchangeHours;
use lean_core::{
    DateTime, Market, OptionRight, OptionStyle, Price, Quantity, Resolution, SettlementType,
    Symbol, SymbolOptionsExt, SymbolProperties, TimeSpan,
};
use lean_data::{CustomDataSubscription, SubscriptionDataConfig, SubscriptionManager};
use lean_options::OptionChain;
use lean_orders::{
    combo_orders::{ComboLegDetails, ComboLegLimitOrder, ComboLimitOrder, ComboMarketOrder},
    order::{Order, OrderType},
    order_ticket::OrderTicket,
    trailing_stop_order::TrailingStopOrderParams,
    transaction_manager::TransactionManager,
    LimitIfTouchedOrder, TrailingStopOrder,
};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::Arc;

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
    pub is_warming_up: bool,

    // Order counter
    order_id_counter: i64,

    // Option tracking
    /// Open option positions keyed by symbol SID.
    pub option_positions: HashMap<u64, OpenOptionPosition>,
    /// Canonical option symbols (e.g. `?SPY`) for chain subscriptions.
    pub option_subscriptions: Vec<Symbol>,
    /// Specific option contracts that have been subscribed to.
    pub open_option_contracts: Vec<Symbol>,
    /// Generated option chains keyed by canonical ticker (e.g. "?SPY").
    pub option_chains: HashMap<String, OptionChain>,

    /// The benchmark symbol set by the algorithm (ticker, e.g. "SPY").
    /// When None, the runner defaults to SPY automatically.
    pub benchmark_symbol: Option<String>,

    /// Custom data subscriptions registered via `add_data()`.
    pub custom_data_subscriptions: Vec<CustomDataSubscription>,
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
            is_warming_up: false,
            order_id_counter: 0,
            option_positions: HashMap::new(),
            option_subscriptions: Vec::new(),
            open_option_contracts: Vec::new(),
            option_chains: HashMap::new(),
            benchmark_symbol: None,
            custom_data_subscriptions: Vec::new(),
        }
    }

    /// Set the benchmark symbol (e.g. "SPY"). When not called, the runner
    /// automatically uses SPY as the default benchmark.
    pub fn set_benchmark(&mut self, ticker: impl Into<String>) {
        self.benchmark_symbol = Some(ticker.into().to_uppercase());
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
    }

    pub fn set_warmup(&mut self, period: TimeSpan) {
        self.warmup_period = Some(period);
    }

    pub fn set_warmup_periods(&mut self, periods: i64, resolution: Resolution) {
        let nanos = resolution
            .to_nanos()
            .unwrap_or(TimeSpan::ONE_DAY.nanos as u64) as i64
            * periods;
        self.warmup_period = Some(TimeSpan::from_nanos(nanos));
    }

    /// Set warm-up by number of bars. During warm-up `on_data` is called but
    /// orders are not processed and equity is not recorded.
    pub fn set_warm_up_bars(&mut self, bar_count: usize) {
        self.warmup_bar_count = Some(bar_count);
        self.is_warming_up = true;
    }

    /// Set warm-up by time period.
    pub fn set_warm_up(&mut self, duration: TimeSpan) {
        self.warmup_duration = Some(duration);
        self.is_warming_up = true;
    }

    /// Called by the engine when warm-up data has been fully replayed.
    pub fn end_warm_up(&mut self) {
        self.is_warming_up = false;
        self.warmup_bar_count = None;
        self.warmup_duration = None;
    }

    // ─── Universe Management ─────────────────────────────────────────────────

    pub fn add_equity(&mut self, ticker: &str, resolution: Resolution) -> Symbol {
        let market = Market::usa();
        let symbol = Symbol::create_equity(ticker, &market);
        let config = SubscriptionDataConfig::new_equity(symbol.clone(), resolution);
        self.subscription_manager.add(config);
        let hours = ExchangeHours::us_equity();
        let props = SymbolProperties::default();
        self.securities.add(crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
        ));
        symbol
    }

    pub fn add_forex(&mut self, ticker: &str, resolution: Resolution) -> Symbol {
        let symbol = Symbol::create_forex(ticker);
        let config = SubscriptionDataConfig::new_forex(symbol.clone(), resolution);
        self.subscription_manager.add(config);
        let hours = ExchangeHours::forex_24h();
        let props = SymbolProperties::default();
        self.securities.add(crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
        ));
        symbol
    }

    pub fn add_crypto(&mut self, ticker: &str, market: &Market, resolution: Resolution) -> Symbol {
        let symbol = Symbol::create_crypto(ticker, market);
        let config = SubscriptionDataConfig::new_crypto(symbol.clone(), resolution);
        self.subscription_manager.add(config);
        let hours = ExchangeHours::crypto_24_7();
        let props = SymbolProperties::default();
        self.securities.add(crate::securities::Security::new(
            symbol.clone(),
            resolution,
            props,
            hours,
        ));
        symbol
    }

    // ─── Ordering ────────────────────────────────────────────────────────────

    fn next_order_id(&mut self) -> i64 {
        self.order_id_counter += 1;
        self.order_id_counter
    }

    pub fn market_order(&mut self, symbol: &Symbol, quantity: Quantity) -> OrderTicket {
        let id = self.next_order_id();
        let order = Order::market(id, symbol.clone(), quantity, self.utc_time, "");
        self.transactions.add_order(order)
    }

    pub fn limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        limit_price: Price,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let order = Order::limit(id, symbol.clone(), quantity, limit_price, self.utc_time, "");
        self.transactions.add_order(order)
    }

    pub fn stop_market_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let order = Order::stop_market(id, symbol.clone(), quantity, stop_price, self.utc_time, "");
        self.transactions.add_order(order)
    }

    pub fn stop_limit_order(
        &mut self,
        symbol: &Symbol,
        quantity: Quantity,
        stop_price: Price,
        limit_price: Price,
    ) -> OrderTicket {
        let id = self.next_order_id();
        let order = Order::stop_limit(
            id,
            symbol.clone(),
            quantity,
            stop_price,
            limit_price,
            self.utc_time,
            "",
        );
        self.transactions.add_order(order)
    }

    /// Market-on-open order.
    pub fn market_on_open_order(&mut self, symbol: &Symbol, quantity: Quantity) -> OrderTicket {
        let id = self.next_order_id();
        let mut order = Order::market(id, symbol.clone(), quantity, self.utc_time, "");
        order.order_type = OrderType::MarketOnOpen;
        self.transactions.add_order(order)
    }

    /// Market-on-close order.
    pub fn market_on_close_order(&mut self, symbol: &Symbol, quantity: Quantity) -> OrderTicket {
        let id = self.next_order_id();
        let mut order = Order::market(id, symbol.clone(), quantity, self.utc_time, "");
        order.order_type = OrderType::MarketOnClose;
        self.transactions.add_order(order)
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
        self.transactions.add_order(tso.order)
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
        self.transactions.add_order(lit.order)
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
        self.transactions.add_order(cmo.order)
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
        self.transactions.add_order(clo.order)
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
        self.transactions.add_order(cll.order)
    }

    /// Set holdings to a target portfolio weight (0.0 = 0%, 1.0 = 100% of portfolio).
    pub fn set_holdings(&mut self, symbol: &Symbol, target: Decimal) -> Option<OrderTicket> {
        let portfolio_value = self.portfolio.total_portfolio_value();
        let current_price = self.securities.get(symbol)?.current_price();

        if current_price.is_zero() {
            return None;
        }

        let target_value = portfolio_value * target;
        let current_holding = self.portfolio.get_holding(symbol);
        let current_value = current_holding.market_value();
        let delta_value = target_value - current_value;

        if delta_value.abs() < dec!(1) {
            return None;
        } // avoid tiny orders

        let qty = delta_value / current_price;
        // Truncate (floor toward zero) to integer, matching C# LEAN's lot-size behavior
        let qty_rounded = qty.trunc();

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

    pub fn sma(&self, period: usize) -> lean_indicators::Sma {
        lean_indicators::Sma::new(period)
    }

    pub fn ema(&self, period: usize) -> lean_indicators::Ema {
        lean_indicators::Ema::new(period)
    }

    pub fn rsi(&self, period: usize) -> lean_indicators::Rsi {
        lean_indicators::Rsi::new(period)
    }

    pub fn macd(&self, fast: usize, slow: usize, signal: usize) -> lean_indicators::Macd {
        lean_indicators::Macd::new(fast, slow, signal)
    }

    pub fn bb(&self, period: usize, k: Decimal) -> lean_indicators::BollingerBands {
        lean_indicators::BollingerBands::new(period, k)
    }

    pub fn atr(&self, period: usize) -> lean_indicators::Atr {
        lean_indicators::Atr::new(period)
    }

    pub fn adx(&self, period: usize) -> lean_indicators::Adx {
        lean_indicators::Adx::new(period)
    }

    pub fn stochastic(&self, k_period: usize, d_period: usize) -> lean_indicators::Stochastic {
        lean_indicators::Stochastic::new(k_period, d_period)
    }

    pub fn roc(&self, period: usize) -> lean_indicators::Roc {
        lean_indicators::Roc::new(period)
    }

    pub fn cci(&self, period: usize) -> lean_indicators::Cci {
        lean_indicators::Cci::new(period)
    }

    pub fn donchian(&self, period: usize) -> lean_indicators::DonchianChannel {
        lean_indicators::DonchianChannel::new(period)
    }

    pub fn keltner(&self, period: usize, multiplier: Decimal) -> lean_indicators::KeltnerChannel {
        lean_indicators::KeltnerChannel::new(period, multiplier)
    }

    pub fn vwap(&self) -> lean_indicators::Vwap {
        lean_indicators::Vwap::new()
    }

    pub fn obv(&self) -> lean_indicators::Obv {
        lean_indicators::Obv::new()
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
        self.portfolio.total_portfolio_value()
    }
    pub fn is_invested(&self, symbol: &Symbol) -> bool {
        self.portfolio.is_invested(symbol)
    }

    // ─── Options ─────────────────────────────────────────────────────────────

    /// Subscribe to the option chain for an underlying equity.
    /// Returns a canonical option Symbol (e.g., `?SPY`) that can be used
    /// to access the option chain in `on_data()`.
    pub fn add_option(&mut self, underlying_ticker: &str, resolution: Resolution) -> Symbol {
        let underlying = self.add_equity(underlying_ticker, resolution);
        let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
        self.option_subscriptions.push(canonical.clone());
        canonical
    }

    /// Subscribe to a specific option contract.
    pub fn add_option_contract(&mut self, symbol: Symbol, resolution: Resolution) -> Symbol {
        // Add the underlying equity subscription if not already tracked.
        if let Some(ref u) = symbol.underlying {
            if !self.securities.contains(u) {
                self.add_equity(&u.permtick, resolution);
            }
        }
        self.open_option_contracts.push(symbol.clone());
        symbol
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
        let total_premium = premium_per_contract * quantity * Decimal::from(100i64);
        *self.portfolio.cash.write() += total_premium;

        if let Some(option_id) = symbol.option_symbol_id() {
            self.option_positions.insert(
                symbol.id.sid,
                OpenOptionPosition {
                    symbol: symbol.clone(),
                    strike: option_id.strike,
                    expiry: option_id.expiry,
                    right: option_id.right,
                    style: option_id.style,
                    settlement: SettlementType::PhysicalDelivery,
                    quantity: -quantity, // negative = short
                    entry_price: premium_per_contract,
                    contract_unit_of_trade: 100,
                },
            );
        }

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
        let total_cost = premium_per_contract * quantity * Decimal::from(100i64);
        *self.portfolio.cash.write() -= total_cost;

        if let Some(option_id) = symbol.option_symbol_id() {
            self.option_positions.insert(
                symbol.id.sid,
                OpenOptionPosition {
                    symbol: symbol.clone(),
                    strike: option_id.strike,
                    expiry: option_id.expiry,
                    right: option_id.right,
                    style: option_id.style,
                    settlement: SettlementType::PhysicalDelivery,
                    quantity, // positive = long
                    entry_price: premium_per_contract,
                    contract_unit_of_trade: 100,
                },
            );
        }

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
        let total_cost = premium_per_contract * quantity * Decimal::from(100i64);
        *self.portfolio.cash.write() -= total_cost;
        self.option_positions.remove(&symbol.id.sid);
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
        let total_premium = premium_per_contract * quantity * Decimal::from(100i64);
        *self.portfolio.cash.write() += total_premium;
        self.option_positions.remove(&symbol.id.sid);
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
    pub fn get_option_positions(&self) -> Vec<&OpenOptionPosition> {
        self.option_positions.values().collect()
    }

    /// Returns the most recently generated option chain for a canonical ticker.
    pub fn get_option_chain(&self, canonical: &str) -> Option<OptionChain> {
        self.option_chains.get(canonical).cloned()
    }
}
