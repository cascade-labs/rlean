use crate::orders::{record_trade_fill, settle_fill_event_bridge};
use crate::runtime_context::AlgorithmRuntimeContext;
use chrono::TimeZone;
use rlean_algorithm::algorithm::{DataDeliveryPayload, SecurityChanges};
use rlean_algorithm::charting::ChartCollection;
use rlean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices, OptionSubscription};
use rlean_algorithm::margin_call::{
    build_margin_call_context, MarginCallModel, MarginCallOrderRequest,
};
use rlean_algorithm::qc_algorithm::BrokerageModel;
use rlean_alpha::AlphaAnalytics;
use rlean_core::{
    DateTime, LeanError, MarketHoursDatabase, Price, Resolution, Result as LeanResult, TimeSpan,
};
use rlean_data::{Slice, SubscriptionDataConfig, SubscriptionDataKind};
use rlean_data_tables::{Bar, QuoteBar, TradeBar, TradeBarData};
use rlean_options::{get_exercise_quantity, is_auto_exercised, OptionContract};
use rlean_orders::{order_processor::OrderProcessor, Order, OrderEvent, OrderType, SlippageModel};
use rlean_statistics::{Trade, TradeBuilder};
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::info;

/// Routes fill slippage through the model attached to each security, matching
/// C# LEAN's `Security.SlippageModel` ownership.
pub struct SecuritySlippageModel {
    algorithm: Arc<Mutex<rlean_algorithm::qc_algorithm::QcAlgorithm>>,
}

impl std::fmt::Debug for SecuritySlippageModel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecuritySlippageModel")
    }
}

impl SecuritySlippageModel {
    pub fn new(algorithm: Arc<Mutex<rlean_algorithm::qc_algorithm::QcAlgorithm>>) -> Self {
        Self { algorithm }
    }
}

impl SlippageModel for SecuritySlippageModel {
    fn get_slippage_amount(&self, order: &Order, bar: &TradeBar) -> Price {
        let model = self
            .algorithm
            .lock()
            .unwrap()
            .securities
            .get(&order.symbol)
            .map(|security| security.slippage_model());
        model
            .map(|model| model.get_slippage_amount(order, bar))
            .unwrap_or_default()
    }
}

pub struct OrderEventProcessing<'a> {
    pub slice: &'a Slice,
    pub option_chains: &'a [(&'a str, &'a rlean_options::OptionChain)],
    pub order_processor: Option<&'a OrderProcessor>,
    pub portfolio: Option<&'a Arc<rlean_algorithm::portfolio::SecurityPortfolioManager>>,
    pub services: &'a mut dyn AlgorithmServices,
    pub all_order_events: &'a mut Vec<OrderEvent>,
    pub trade_builder: &'a mut TradeBuilder,
    pub completed_trades: &'a mut Vec<Trade>,
}

pub struct AlgorithmManager<B>
where
    B: AlgorithmBridge,
{
    pub algorithm: B,
    runtime_context: AlgorithmRuntimeContext,
    last_date: Option<chrono::NaiveDate>,
    trading_days: i64,
    slices_processed: i64,
    // LEAN fill models read from `Security.Cache`, not only from the current
    // Slice. Retain the last market datum so minute/second/tick market orders
    // submitted between updates can fill synchronously from that cache.
    cached_bars: HashMap<u64, TradeBar>,
    cached_quote_bars: HashMap<u64, QuoteBar>,
}

impl<B> AlgorithmManager<B>
where
    B: AlgorithmBridge,
{
    pub fn new(algorithm: B, runtime_context: AlgorithmRuntimeContext) -> Self {
        AlgorithmManager {
            algorithm,
            runtime_context,
            last_date: None,
            trading_days: 0,
            slices_processed: 0,
            cached_bars: HashMap::new(),
            cached_quote_bars: HashMap::new(),
        }
    }

    pub fn initialize(&mut self, services: &mut dyn AlgorithmServices) -> anyhow::Result<()> {
        info!("Initializing algorithm: {}", self.algorithm.name());
        self.algorithm.initialize(services)
    }

    pub fn framework(&self) -> Arc<Mutex<crate::framework::FrameworkState>> {
        self.runtime_context.framework()
    }

    pub fn set_market_hours_database(&mut self, market_hours_database: Arc<MarketHoursDatabase>) {
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            algorithm_state
                .lock()
                .unwrap()
                .set_market_hours_database(market_hours_database);
        }
    }

    /// Apply the deployment brokerage model before the strategy initializes.
    /// This mirrors LEAN's setup-handler configuration of `IBrokerageModel`, so
    /// securities added by `Initialize` receive the correct buying-power model
    /// and leverage from their first initialization.
    pub fn set_brokerage_model(&mut self, brokerage_model: BrokerageModel) {
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            algorithm_state
                .lock()
                .unwrap()
                .set_brokerage_model(brokerage_model.brokerage, brokerage_model.account_type);
        }
    }

    pub fn start_date(&self) -> DateTime {
        self.algorithm.start_date()
    }

    pub fn end_date(&self) -> DateTime {
        self.algorithm.end_date()
    }

    pub fn starting_cash(&self) -> rlean_core::Price {
        self.algorithm.starting_cash()
    }

    pub fn subscriptions(&self) -> Vec<SubscriptionDataConfig> {
        self.algorithm
            .subscriptions()
            .into_iter()
            .map(|config| (*config).clone())
            .collect()
    }

    pub fn benchmark_symbol(&self) -> String {
        self.algorithm
            .algorithm_state()
            .and_then(|state| state.lock().unwrap().benchmark_symbol.clone())
            .unwrap_or_else(|| "SPY".to_string())
    }

    pub fn option_subscriptions(&self) -> Vec<OptionSubscription> {
        self.algorithm.option_subscriptions()
    }

    pub fn alpha_analytics(&self) -> AlphaAnalytics {
        // The framework state (owned by the runtime context, shared with the pipeline)
        // accumulates every closed insight's realised forward return across the backtest.
        // Reduce that into the IC / correlation / ranking bundle here — the algorithm
        // bridges (Python + native) intentionally return an empty default, so reading it
        // from them drops all diagnostics. Fall back to the bridge only when the framework
        // tracked nothing (e.g. a non-framework algorithm).
        let framework_analytics = self
            .runtime_context
            .framework()
            .lock()
            .unwrap()
            .compute_alpha_analytics();
        if framework_analytics.ranking.is_empty() && framework_analytics.ic_series.is_empty() {
            self.algorithm.alpha_analytics()
        } else {
            framework_analytics
        }
    }

    pub fn charts(&self) -> ChartCollection {
        self.algorithm.charts()
    }

    pub fn prepare_data_delivery(
        &mut self,
        subscriptions: &[SubscriptionDataConfig],
    ) -> anyhow::Result<()> {
        self.algorithm.prepare_data_delivery(subscriptions)
    }

    pub fn portfolio_value(&self) -> rlean_core::Price {
        self.algorithm.portfolio_value()
    }

    pub fn transactions(&self) -> Option<Arc<rlean_orders::TransactionManager>> {
        self.algorithm.transactions()
    }

    pub fn portfolio(&self) -> Option<Arc<rlean_algorithm::portfolio::SecurityPortfolioManager>> {
        self.algorithm.portfolio()
    }

    /// Scan the default portfolio margin-call model using the current
    /// algorithm state. C# LEAN performs this scan every five minutes after
    /// scheduled events and before OnData.
    pub fn margin_call_requests(
        &self,
        time: DateTime,
    ) -> (Vec<MarginCallOrderRequest>, bool, bool) {
        let Some(state) = self.algorithm.algorithm_state() else {
            return (Vec::new(), false, false);
        };
        let Some(portfolio) = self.algorithm.portfolio() else {
            return (Vec::new(), false, false);
        };
        let algorithm = state.lock().unwrap();
        let model = portfolio.margin_call_model();
        if model.is_null() {
            return (Vec::new(), false, false);
        }
        let context = build_margin_call_context(&portfolio, &algorithm);
        let (requests, warning) = model.get_margin_call_orders(&context);
        let exchanges_open = requests.iter().all(|request| {
            algorithm
                .securities
                .get(&request.symbol)
                .map(|security| security.exchange_hours.is_open_at(time))
                .unwrap_or(false)
        });
        (requests, warning, exchanges_open)
    }

    pub fn margin_remaining(&self) -> Option<Price> {
        self.algorithm
            .algorithm_state()
            .map(|state| state.lock().unwrap().margin_remaining())
    }

    pub fn notify_margin_call(
        &mut self,
        requests: &[MarginCallOrderRequest],
        time: DateTime,
        services: &mut dyn AlgorithmServices,
    ) {
        let orders = requests
            .iter()
            .map(|request| {
                Order::market(
                    0,
                    request.symbol.clone(),
                    request.quantity,
                    time,
                    &request.tag,
                )
            })
            .collect::<Vec<_>>();
        self.algorithm.on_margin_call(&orders, services);
    }

    pub fn notify_margin_call_warning(&mut self, services: &mut dyn AlgorithmServices) {
        self.algorithm.on_margin_call_warning(services);
    }

    pub fn submit_margin_call_order(&mut self, request: &MarginCallOrderRequest) -> Option<i64> {
        let state = self.algorithm.algorithm_state()?;
        let mut algorithm = state.lock().unwrap();
        let ticket = algorithm.market_order_with_options_and_tag(
            &request.symbol,
            request.quantity,
            None,
            false,
            request.tag,
        );
        Some(ticket.order_id)
    }

    pub fn algorithm(&self) -> &B {
        &self.algorithm
    }

    pub fn trading_days(&self) -> i64 {
        self.trading_days
    }

    /// Current algorithm-local calendar date. LEAN advances its UTC frontier but
    /// exposes and compares dates in `QCAlgorithm.TimeZone` (New York by default).
    pub fn current_date(&self) -> Option<chrono::NaiveDate> {
        self.last_date
    }

    /// Convert an algorithm-local calendar boundary to UTC. LEAN interprets
    /// SetStartDate/SetEndDate in QCAlgorithm.TimeZone, not as UTC dates.
    pub fn local_midnight_utc(&self, date: chrono::NaiveDate) -> LeanResult<DateTime> {
        let time_zone = self
            .algorithm
            .algorithm_state()
            .map(|state| state.lock().unwrap().time_zone)
            .unwrap_or(chrono_tz::America::New_York);
        let local = date
            .and_hms_opt(0, 0, 0)
            .expect("valid algorithm-local midnight");
        let zoned = time_zone
            .from_local_datetime(&local)
            .single()
            .or_else(|| time_zone.from_local_datetime(&local).earliest())
            .ok_or_else(|| {
                LeanError::DataError(format!(
                    "algorithm-local midnight does not exist: date={date}, timezone={time_zone}"
                ))
            })?;
        Ok(DateTime::from(zoned.with_timezone(&chrono::Utc)))
    }

    /// Whether at least one active non-custom subscription can trade on the
    /// algorithm-local date. LEAN advances custom data independently, but its
    /// exchange-date keepers and result sampling only emit dates open for the
    /// associated market subscription.
    pub fn is_trading_date(&self, date: chrono::NaiveDate) -> bool {
        let Some(state) = self.algorithm.algorithm_state() else {
            return true;
        };
        let algorithm = state.lock().unwrap();
        let market_configs = algorithm
            .subscription_manager
            .get_all()
            .into_iter()
            .filter(|config| config.data_kind != SubscriptionDataKind::Custom)
            .collect::<Vec<_>>();
        market_configs.is_empty()
            || market_configs.iter().any(|config| {
                let exchange_symbol = config
                    .symbol
                    .underlying
                    .as_deref()
                    .unwrap_or(&config.symbol);
                algorithm
                    .market_hours_database
                    .exchange_hours(exchange_symbol)
                    .session_bounds(date)
                    .is_some()
            })
    }

    pub fn slices_processed(&self) -> i64 {
        self.slices_processed
    }

    pub fn handle_new_trading_day(
        &mut self,
        slice: &Slice,
        services: &mut dyn AlgorithmServices,
    ) -> LeanResult<bool> {
        let slice_date = algorithm_local_date(self.algorithm.algorithm_state(), slice.time);
        let new_trading_day = trading_day_transition(
            self.last_date,
            slice_date,
            self.is_trading_date(slice_date),
            slice.time,
        )?;
        if new_trading_day {
            if self.last_date.is_some() {
                self.algorithm.on_end_of_day(None, services);
            }
            self.trading_days += 1;
        }
        self.last_date = Some(slice_date);
        self.slices_processed += 1;
        Ok(new_trading_day)
    }

    pub fn apply_universe_selection(
        &mut self,
        slice: &Slice,
        new_trading_day: bool,
        services: &mut dyn AlgorithmServices,
    ) -> SecurityChanges {
        let mut changes = SecurityChanges::empty();
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            let mut algorithm = algorithm_state.lock().unwrap();
            for (canonical_key, chain) in &slice.option_chains {
                let canonical = algorithm
                    .option_subscriptions
                    .iter()
                    .find(|symbol| symbol.permtick.as_ref() == canonical_key)
                    .cloned();
                if let Some(canonical) = canonical {
                    let option_changes =
                        algorithm.apply_option_universe_membership(&canonical, chain.as_ref());
                    changes.added.extend(option_changes.added);
                    changes.removed.extend(option_changes.removed);
                }
            }
        }
        if !self.algorithm.has_universes() && changes.has_changes() {
            self.algorithm.on_securities_changed(&changes, services);
            crate::notify_framework_securities_changed(
                &self.runtime_context.framework(),
                &changes.added,
                &changes.removed,
            );
            return changes;
        } else if !self.algorithm.has_universes() {
            return changes;
        }

        let resolution = self
            .algorithm
            .universe_resolution()
            .unwrap_or(rlean_core::Resolution::Daily);
        let mut selection_pass = false;
        if new_trading_day {
            let selections =
                self.algorithm
                    .select_universe_changes(slice.time.0, resolution, services);
            if !selections.is_empty() {
                selection_pass = true;
                if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                    algorithm_state
                        .lock()
                        .unwrap()
                        .begin_universe_selection_pass();
                }
            }
            for selection in selections {
                if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                    let mut algorithm = algorithm_state.lock().unwrap();
                    crate::algorithm_services::apply_universe_changes(
                        &mut algorithm,
                        &mut changes,
                        selection.changes,
                        selection.resolution,
                    );
                } else {
                    changes.added.extend(selection.changes.added);
                    changes.removed.extend(selection.changes.removed);
                }
            }
        }
        if !slice.custom_data.is_empty() {
            let selections = self.algorithm.select_custom_universe_changes(
                slice.time.0,
                resolution,
                &slice.custom_data,
                services,
            );
            if !selection_pass && !selections.is_empty() {
                selection_pass = true;
                if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                    algorithm_state
                        .lock()
                        .unwrap()
                        .begin_universe_selection_pass();
                }
            }
            for selection in selections {
                if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                    let mut algorithm = algorithm_state.lock().unwrap();
                    crate::algorithm_services::apply_custom_universe_changes(
                        &mut algorithm,
                        &mut changes,
                        selection.changes,
                        selection.resolution,
                        &slice.custom_data,
                    );
                } else {
                    changes.added.extend(selection.changes.added);
                    changes.removed.extend(selection.changes.removed);
                }
            }
        }
        if !slice.fundamentals.is_empty() {
            let selections = self.algorithm.select_fundamental_universe_changes(
                slice.time.0,
                resolution,
                &slice.fundamentals,
                services,
            );
            if !selection_pass && !selections.is_empty() {
                selection_pass = true;
                if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                    algorithm_state
                        .lock()
                        .unwrap()
                        .begin_universe_selection_pass();
                }
            }
            for selection in selections {
                if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                    let mut algorithm = algorithm_state.lock().unwrap();
                    // The fundamental selector uses the normal equity
                    // subscription path; unlike custom universes it has no
                    // per-symbol metadata to attach.
                    crate::algorithm_services::apply_universe_changes(
                        &mut algorithm,
                        &mut changes,
                        selection.changes,
                        selection.resolution,
                    );
                } else {
                    changes.added.extend(selection.changes.added);
                    changes.removed.extend(selection.changes.removed);
                }
            }
        }
        if selection_pass {
            if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                // C# PendingRemovalsManager checks removals from an earlier
                // selection only after the current selections are known. The
                // add path above cancels a removal when the symbol is selected
                // again; newly removed symbols cannot disappear this pass.
                algorithm_state
                    .lock()
                    .unwrap()
                    .process_pending_universe_security_removals();
            }
        }
        if changes.has_changes() {
            // Match C# AlgorithmManager: the user callback and every framework
            // model receive the same time-slice SecurityChanges before OnData.
            // Without this notification the framework can retain insights and
            // execution targets for a universe member after it is removed.
            self.algorithm.on_securities_changed(&changes, services);
            crate::notify_framework_securities_changed(
                &self.runtime_context.framework(),
                &changes.added,
                &changes.removed,
            );
        }
        changes
    }

    /// Add the algorithm's active option chain to slices containing concrete
    /// contract data. C# LEAN's `TimeSliceFactory.HandleOptionData` rebuilds the
    /// chain on every contract update; rlean's universe snapshot and contract
    /// streams are separate, so this joins them before pricing and delivery.
    pub fn include_active_option_chains(&self, slice: &mut Slice) {
        let Some(algorithm_state) = self.algorithm.algorithm_state() else {
            return;
        };
        let algorithm = algorithm_state.lock().unwrap();
        let chains = algorithm
            .option_subscriptions
            .iter()
            .filter_map(|canonical| {
                let key = canonical.permtick.to_string();
                if slice.option_chains.contains_key(&key) {
                    return None;
                }
                let chain = algorithm.get_option_chain(&key)?;
                let has_contract_data = chain.contracts.keys().any(|symbol| {
                    slice.quote_bars.contains_key(&symbol.id.sid)
                        || slice.bars.contains_key(&symbol.id.sid)
                        || slice.ticks.contains_key(&symbol.id.sid)
                });
                has_contract_data.then_some((key, Arc::new(chain)))
            })
            .collect::<Vec<_>>();
        drop(algorithm);
        slice.option_chains.extend(chains);
    }

    pub fn advance_frontier(&mut self, slice: &Slice, _services: &mut dyn AlgorithmServices) {
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            let mut algorithm = algorithm_state.lock().unwrap();
            crate::algorithm_services::advance_algorithm_time(&mut algorithm, slice.time);
            crate::algorithm_services::apply_slice_security_prices(&mut algorithm, slice);
        }
        self.cached_bars
            .extend(slice.bars.iter().map(|(sid, bar)| (*sid, bar.clone())));
        self.cached_quote_bars.extend(
            slice
                .quote_bars
                .iter()
                .map(|(sid, quote)| (*sid, quote.clone())),
        );
        self.runtime_context.update_registered_indicators(slice);
    }

    pub fn prime_scheduled_events(&self, utc_time: DateTime) {
        self.runtime_context.schedule().skip_until(utc_time);
    }

    /// Fire engine-owned scheduled events through `utc_time`. The algorithm
    /// clock is set to each event's exact UTC trigger before its callback, as in
    /// C# LEAN's BacktestingRealTimeHandler.ScanPastEvents. The subsequent data
    /// frontier advance restores the slice time.
    pub fn scan_scheduled_events(&mut self, utc_time: DateTime) -> anyhow::Result<Vec<DateTime>> {
        let market_hours_database = self
            .algorithm
            .algorithm_state()
            .map(|state| state.lock().unwrap().market_hours_database.clone())
            .unwrap_or_else(MarketHoursDatabase::global);
        let due = self
            .runtime_context
            .schedule()
            .due_events(utc_time, market_hours_database.as_ref());
        let mut fired = Vec::with_capacity(due.len());
        for event in due {
            if let Some(algorithm_state) = self.algorithm.algorithm_state() {
                crate::algorithm_services::advance_algorithm_time(
                    &mut algorithm_state.lock().unwrap(),
                    event.trigger_time,
                );
            }
            let result = (event.callback.lock())();
            if let Err(error) = result {
                anyhow::bail!(
                    "scheduled event '{}' failed at {}: {}",
                    event.name,
                    event.trigger_time,
                    error
                );
            }
            fired.push(event.trigger_time);
        }
        Ok(fired)
    }

    pub fn process_order_events(&mut self, processing: OrderEventProcessing<'_>) {
        let OrderEventProcessing {
            slice,
            option_chains,
            order_processor,
            portfolio,
            services,
            all_order_events,
            trade_builder,
            completed_trades,
        } = processing;
        let (Some(processor), Some(portfolio)) = (order_processor, portfolio) else {
            return;
        };
        if !processor.transaction_manager.has_open_orders() {
            return;
        }
        let mut bars = slice
            .bars
            .iter()
            .map(|(sid, bar)| (*sid, bar.clone()))
            .collect();
        extend_bars_with_option_contracts(&mut bars, slice, option_chains);
        // Option contracts live in chains, not slice.quote_bars; synthesize
        // per-contract quote bars so market fills pay the spread (buy at ask,
        // sell at bid) instead of falling back to the synthesized trade bar.
        let mut quote_bars = slice.quote_bars.clone();
        extend_quote_bars_with_option_contracts(&mut quote_bars, slice, option_chains);
        self.extend_with_cached_market_order_data(
            processor,
            slice.time,
            &mut bars,
            &mut quote_bars,
        );
        let mut events =
            processor.generate_order_events_with_quotes(&bars, &quote_bars, slice.time);
        for event in events.iter_mut() {
            let fee = settle_fill_event_bridge(&self.algorithm, portfolio, processor, event);
            processor
                .transaction_manager
                .process_order_event(event.clone());
            self.algorithm.on_order_event(event, services);
            all_order_events.push(event.clone());
            if let Some(fee) = fee {
                record_trade_fill(trade_builder, completed_trades, event, fee);
            }
        }
    }

    /// Match C# LEAN `FillModel.InternalMarketFill`: fine-resolution market
    /// orders may fill from the security cache when the current Slice has no
    /// row for their symbol. Hour/daily subscriptions still wait for fresh
    /// data, matching `ShouldWaitForFreshData`.
    fn extend_with_cached_market_order_data(
        &self,
        processor: &OrderProcessor,
        time: DateTime,
        bars: &mut HashMap<u64, TradeBar>,
        quote_bars: &mut HashMap<u64, QuoteBar>,
    ) {
        let algorithm_state = self.algorithm.algorithm_state();
        let algorithm = algorithm_state.as_ref().map(|state| state.lock().unwrap());
        for order in processor.transaction_manager.get_open_orders() {
            if order.order_type != OrderType::Market {
                continue;
            }
            let sid = order.symbol.id.sid;
            if bars.contains_key(&sid) || quote_bars.contains_key(&sid) {
                continue;
            }
            let Some(security) = algorithm
                .as_ref()
                .and_then(|algorithm| algorithm.securities.get(&order.symbol))
            else {
                continue;
            };
            if matches!(security.resolution, Resolution::Hour | Resolution::Daily)
                || !security.exchange_hours.is_open_at(time)
            {
                continue;
            }
            if let Some(quote) = self.cached_quote_bars.get(&sid) {
                quote_bars.insert(sid, quote.clone());
            } else if let Some(bar) = self.cached_bars.get(&sid) {
                bars.insert(sid, bar.clone());
            }
        }
    }

    pub fn process_option_expirations(
        &mut self,
        slice: &Slice,
        services: &mut dyn AlgorithmServices,
    ) {
        let Some(algorithm_state) = self.algorithm.algorithm_state() else {
            return;
        };
        let Some(portfolio) = self.algorithm.portfolio() else {
            return;
        };

        let (positions, market_hours_database) = {
            let algorithm = algorithm_state.lock().unwrap();
            (
                algorithm.get_option_positions(),
                algorithm.market_hours_database.clone(),
            )
        };
        for position in positions {
            if !market_hours_database.is_option_contract_expired(&position.symbol, slice.time) {
                continue;
            }

            let Some(underlying) = position.symbol.underlying.clone() else {
                continue;
            };
            let Some(underlying_price) = slice.bars.get(&underlying.id.sid).map(|bar| bar.close)
            else {
                continue;
            };

            let holding = portfolio.get_holding(&position.symbol);
            if !holding.is_invested() {
                continue;
            }

            let mut contract = OptionContract::new(position.symbol.clone());
            contract.data.underlying_last_price = underlying_price;
            contract.data.last_price = dec!(0);

            if is_auto_exercised(underlying_price, position.strike, position.right) {
                let exercise_quantity = get_exercise_quantity(
                    -position.quantity,
                    position.right,
                    position.contract_unit_of_trade,
                );
                portfolio.apply_exercise_with_market_price(
                    &underlying,
                    position.strike,
                    exercise_quantity,
                    underlying_price,
                );
                portfolio.apply_fill_with_multiplier(
                    &position.symbol,
                    dec!(0),
                    -position.quantity,
                    dec!(0),
                    dec!(100),
                );
                self.algorithm.on_assignment_order_event(
                    contract,
                    position.quantity,
                    position.quantity < dec!(0),
                    services,
                );
            } else {
                let entry_premium = holding.average_price;
                portfolio.apply_fill_with_multiplier(
                    &position.symbol,
                    dec!(0),
                    -position.quantity,
                    dec!(0),
                    dec!(100),
                );
                self.algorithm.on_otm_expiry(
                    contract,
                    position.quantity,
                    underlying_price,
                    entry_premium,
                    services,
                );
            }
        }
    }

    pub fn deliver_data(
        &mut self,
        payload: DataDeliveryPayload,
        services: &mut dyn AlgorithmServices,
    ) {
        let slice = payload.slice.as_ref();
        if !slice.splits.is_empty() {
            self.algorithm.on_splits(&slice.splits, services);
        }
        if !slice.dividends.is_empty() {
            self.algorithm.on_dividends(&slice.dividends, services);
        }
        if !slice.delistings.is_empty() {
            self.algorithm.on_delistings(&slice.delistings, services);
        }
        if !slice.symbol_changed_events.is_empty() {
            self.algorithm
                .on_symbol_changed_events(&slice.symbol_changed_events, services);
        }
        self.algorithm.on_data(payload, services);
    }

    pub fn run_framework(&mut self, slice: &Slice, _services: &mut dyn AlgorithmServices) {
        let order_requests = if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            crate::run_framework_pipeline(
                &self.runtime_context.framework(),
                &algorithm_state,
                slice,
            )
        } else {
            Vec::new()
        };
        tracing::debug!(
            "framework pipeline produced {} order request(s) at {:?}",
            order_requests.len(),
            slice.time
        );
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            crate::algorithm_services::submit_execution_order_requests(
                &algorithm_state,
                order_requests,
            );
        }
    }

    pub fn advance_framework_warmup(
        &mut self,
        slice: &Slice,
        _services: &mut dyn AlgorithmServices,
    ) {
        // LEAN advances framework models during warmup, but order submission is
        // rejected while IsWarmingUp. Drop execution requests here so alpha/PCM
        // state is warmed without creating tradable orders.
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            let _ = crate::run_framework_pipeline(
                &self.runtime_context.framework(),
                &algorithm_state,
                slice,
            );
        }
    }

    /// Process one historical warm-up slice through the same algorithm and
    /// framework state paths used by a normal time step, while suppressing
    /// executable framework orders until warm-up completes.
    pub fn process_warmup_slice(
        &mut self,
        slice: Arc<Slice>,
        services: &mut dyn AlgorithmServices,
    ) -> anyhow::Result<()> {
        self.advance_frontier(slice.as_ref(), services);
        self.deliver_data(
            DataDeliveryPayload {
                slice: slice.clone(),
            },
            services,
        );
        if let Some(error) = self.algorithm.runtime_error() {
            anyhow::bail!("Algorithm runtime error during warm-up: {error}");
        }
        self.advance_framework_warmup(slice.as_ref(), services);
        self.end_time_step(services);
        Ok(())
    }

    pub fn end_time_step(&mut self, services: &mut dyn AlgorithmServices) {
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            let logical_removals = algorithm_state
                .lock()
                .unwrap()
                .take_pending_removed_security_changes();
            if !logical_removals.is_empty() {
                crate::notify_framework_securities_changed(
                    &self.runtime_context.framework(),
                    &[],
                    &logical_removals,
                );
                self.algorithm.on_securities_changed(
                    &rlean_algorithm::algorithm::SecurityChanges {
                        added: Vec::new(),
                        removed: logical_removals,
                    },
                    services,
                );
            }

            // Direct RemoveSecurity calls use LEAN's user-defined-universe
            // path and can physically detach at end of time step. Universe
            // selection removals are reconsidered only by a later selection.
            let mut algorithm = algorithm_state.lock().unwrap();
            algorithm.process_pending_direct_security_removals();
            algorithm.advance_removal_time_step();
        }
        self.algorithm.on_end_of_time_step(services);
    }

    pub fn warmup_finished(&mut self, services: &mut dyn AlgorithmServices) {
        if let Some(algorithm_state) = self.algorithm.algorithm_state() {
            algorithm_state.lock().unwrap().end_warm_up();
        }
        self.algorithm.on_warmup_finished(services);
    }

    pub fn is_warming_up(&self) -> bool {
        self.algorithm
            .algorithm_state()
            .map(|state| state.lock().unwrap().is_warming_up)
            .unwrap_or(false)
    }

    pub fn warmup_duration(&self) -> Option<TimeSpan> {
        self.algorithm.algorithm_state().and_then(|state| {
            let algorithm = state.lock().unwrap();
            algorithm
                .warmup_duration
                .or(algorithm.warmup_period)
                .or_else(|| {
                    let bars = algorithm.warmup_bar_count?;
                    let resolution = algorithm.warmup_resolution.unwrap_or(Resolution::Minute);
                    resolution.to_time_span().map(|span| TimeSpan {
                        nanos: span.nanos.saturating_mul(bars as i64),
                    })
                })
        })
    }

    /// Number of bars requested via `SetWarmUp(barCount, ...)`, if any.
    ///
    /// A bar-count warmup must be sized against the exchange calendar (N
    /// trading sessions), not a naive calendar-day span, so the runner handles
    /// it separately from `warmup_duration`.
    pub fn warmup_bar_count(&self) -> Option<usize> {
        self.algorithm.algorithm_state().and_then(|state| {
            let algorithm = state.lock().unwrap();
            // Only bar-count warmups need calendar-aware sizing; explicit
            // durations/periods are already calendar spans.
            if algorithm.warmup_duration.is_some() || algorithm.warmup_period.is_some() {
                None
            } else {
                algorithm.warmup_bar_count
            }
        })
    }

    pub fn warmup_resolution(&self) -> Option<Resolution> {
        self.algorithm
            .algorithm_state()
            .and_then(|state| state.lock().unwrap().warmup_resolution)
    }

    pub fn finish(&mut self, services: &mut dyn AlgorithmServices) {
        if self.slices_processed > 0 {
            self.algorithm.on_end_of_day(None, services);
        }
        tracing::debug!(
            "backtest loop processed {} slices across {} trading days",
            self.slices_processed,
            self.trading_days
        );
        self.algorithm.on_end_of_algorithm(services);
    }
}

fn trading_day_transition(
    previous: Option<chrono::NaiveDate>,
    incoming: chrono::NaiveDate,
    incoming_is_open: bool,
    slice_time: DateTime,
) -> LeanResult<bool> {
    match previous {
        Some(previous) if incoming < previous => Err(LeanError::DataError(format!(
            "algorithm time moved backward across trading days: previous={previous}, incoming={incoming}, slice_time={slice_time}"
        ))),
        Some(previous) => Ok(incoming > previous && incoming_is_open),
        None => Ok(incoming_is_open),
    }
}

fn algorithm_local_date(
    algorithm_state: Option<Arc<Mutex<rlean_algorithm::qc_algorithm::QcAlgorithm>>>,
    utc_time: DateTime,
) -> chrono::NaiveDate {
    algorithm_state
        .map(|state| state.lock().unwrap().local_date(utc_time))
        .unwrap_or_else(|| utc_time.to_tz(chrono_tz::America::New_York).date_naive())
}

/// Synthesize quote bars for option contracts from their chain bid/ask so the
/// fill model prices market orders at the quote side. Contracts without any
/// positive quote are skipped and fall back to the synthesized trade bar.
fn extend_quote_bars_with_option_contracts(
    quote_bars: &mut std::collections::HashMap<u64, QuoteBar>,
    slice: &Slice,
    option_chains: &[(&str, &rlean_options::OptionChain)],
) {
    for (_, chain) in option_chains {
        for contract in chain.contracts.values() {
            let bid = contract.data.bid_price;
            let ask = contract.data.ask_price;
            if bid <= dec!(0) && ask <= dec!(0) {
                continue;
            }
            let side =
                |price: rust_decimal::Decimal| (price > dec!(0)).then(|| Bar::from_price(price));
            quote_bars.insert(
                contract.symbol.id.sid,
                QuoteBar::new(
                    contract.symbol.clone(),
                    slice.time,
                    TimeSpan::from_days(1),
                    side(bid),
                    side(ask),
                    contract.data.bid_size.into(),
                    contract.data.ask_size.into(),
                ),
            );
        }
    }
}

fn extend_bars_with_option_contracts(
    bars: &mut std::collections::HashMap<u64, TradeBar>,
    slice: &Slice,
    option_chains: &[(&str, &rlean_options::OptionChain)],
) {
    for (_, chain) in option_chains {
        for contract in chain.contracts.values() {
            let price = contract
                .mid_price()
                .max(contract.data.last_price)
                .max(contract.data.bid_price)
                .max(contract.data.ask_price);
            if price <= dec!(0) {
                continue;
            }

            bars.insert(
                contract.symbol.id.sid,
                TradeBar::new(
                    contract.symbol.clone(),
                    slice.time,
                    TimeSpan::from_days(1),
                    TradeBarData::new(price, price, price, price, contract.data.volume.into()),
                ),
            );
        }
    }
}

#[cfg(test)]
mod trading_day_tests {
    use super::*;

    #[test]
    fn midnight_utc_does_not_start_a_new_algorithm_day() {
        let state = Arc::new(Mutex::new(rlean_algorithm::qc_algorithm::QcAlgorithm::new(
            "test",
            dec!(100_000),
        )));
        let before_utc_midnight = DateTime::from_secs(1_723_852_740); // 2024-08-16 23:59 UTC
        let at_utc_midnight = DateTime::from_secs(1_723_852_800); // 2024-08-17 00:00 UTC
        let at_new_york_midnight = DateTime::from_secs(1_723_867_200); // 2024-08-17 04:00 UTC

        let friday = algorithm_local_date(Some(state.clone()), before_utc_midnight);
        assert_eq!(
            algorithm_local_date(Some(state.clone()), at_utc_midnight),
            friday
        );
        assert!(!trading_day_transition(Some(friday), friday, true, at_utc_midnight).unwrap());

        let saturday = algorithm_local_date(Some(state), at_new_york_midnight);
        assert_ne!(saturday, friday);
        assert!(
            !trading_day_transition(Some(friday), saturday, false, at_new_york_midnight).unwrap()
        );
    }

    #[test]
    fn backward_date_errors_before_it_can_increment_the_day_count() {
        let august = chrono::NaiveDate::from_ymd_opt(2025, 8, 21).unwrap();
        let december = chrono::NaiveDate::from_ymd_opt(2025, 12, 24).unwrap();
        let mut trading_days = 0;

        if trading_day_transition(None, august, true, DateTime::from_secs(1_000)).unwrap() {
            trading_days += 1;
        }
        if trading_day_transition(Some(august), december, true, DateTime::from_secs(2_000)).unwrap()
        {
            trading_days += 1;
        }
        let error =
            trading_day_transition(Some(december), august, true, DateTime::from_secs(1_000))
                .unwrap_err();

        assert_eq!(trading_days, 2);
        assert!(error
            .to_string()
            .contains("algorithm time moved backward across trading days"));
    }

    #[test]
    fn repeated_slices_on_one_date_do_not_double_count_the_day() {
        let date = chrono::NaiveDate::from_ymd_opt(2025, 12, 24).unwrap();

        assert!(trading_day_transition(None, date, true, DateTime::from_secs(1_000)).unwrap());
        assert!(
            !trading_day_transition(Some(date), date, true, DateTime::from_secs(2_000)).unwrap()
        );
    }
}
