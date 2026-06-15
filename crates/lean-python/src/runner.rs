/// Standalone Python strategy runner.
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::{Datelike, Local, NaiveDate};
use pyo3::prelude::*;
use pyo3::types::PyType;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json;
use tracing::{debug, info, warn};

use lean_algorithm::algorithm::IAlgorithm;
use lean_algorithm::{
    portfolio::{SecurityHolding, SecurityPortfolioManager},
    qc_algorithm::{AccountType, BrokerageName, OptionFilter, QcAlgorithm},
};
use lean_brokerages::Brokerage;
use lean_core::{
    exchange_hours::ExchangeHours, DataNormalizationMode, DateTime, LeanError, Market, OptionRight,
    OptionStyle, Resolution, SecurityType, Symbol, SymbolOptionsExt, TickType, TimeSpan,
};
use lean_data::split::SplitType;
use lean_data::{
    live_data_channel, CustomDataConfig, CustomDataFormat, CustomDataPoint, CustomDataQuery,
    CustomDataSubscription, CustomDataTransport, Delisting, DelistingType, IHistoricalDataProvider,
    LiveDataItem, LiveDataSubscription, LiveDataSubscriptionConfig, LiveSubscriptionKey,
    LiveUniverseSubscriptionConfig, MarginInterestRate, PerpetualContext, QuoteBar, Slice, Split,
    SubscriptionDataConfig, SymbolChangedEvent, Tick, TradeBar, TradeBarData,
};
use lean_live::{AccountState, AccountSynchronizer, DataQueueHandlerManager, LiveSliceAssembler};
use lean_options::payoff::{get_exercise_quantity, intrinsic_value};
use lean_options::{
    evaluate_contract_with_market_iv, BlackScholesPriceModel, OptionChain, OptionContract,
    OptionContractData,
};
use lean_orders::{
    fill_model::ImmediateFillModel, order::Order, order::OrderStatus, order_event::OrderEvent,
    order_processor::OrderProcessor, slippage::NullSlippageModel,
};
use lean_statistics::{PortfolioStatistics, Statistics, Trade, TradeBuilder};
use lean_storage::{
    custom_data_history_path, custom_data_path, DataCache, FactorFileEntry, MapFileEntry,
    OptionEodBar, OptionUniverseRow, ParquetReader, ParquetWriter, PathResolver, QueryParams,
    WriterConfig,
};

use crate::charting::ChartCollection;
use crate::py_adapter::{set_algorithm_time, PyAlgorithmAdapter};
use crate::py_data::SliceProxy;
use crate::py_framework::run_framework_pipeline;
use crate::py_qc_algorithm::AlgorithmHistoryContext;
use lean_data_providers::{
    DataType, HistoryBatchRequest, IHistoryProvider as SyncHistoryProvider, TickStream,
};

const HIGH_RESOLUTION_PREFETCH_CONCURRENCY: usize = 8;
const SUBSCRIPTION_PREFETCH_CONCURRENCY: usize = 8;
const OPTION_RUNTIME_PREFETCH_CONCURRENCY: usize = 8;
const LIVE_SYNCHRONIZER_HEARTBEAT: Duration = Duration::from_secs(1);

type OptionRuntimeInputs = (
    Vec<Symbol>,
    HashMap<String, OptionFilter>,
    HashMap<String, Resolution>,
    Vec<Symbol>,
);

fn default_backtest_end_date(today: NaiveDate) -> NaiveDate {
    today - chrono::Duration::days(1)
}

fn resolve_backtest_end_date(
    end_date_override: Option<NaiveDate>,
    strategy_end_date: DateTime,
    today: NaiveDate,
) -> NaiveDate {
    end_date_override.unwrap_or_else(|| {
        if strategy_end_date == DateTime::MAX {
            default_backtest_end_date(today)
        } else {
            strategy_end_date.date_utc()
        }
    })
}

struct SubscriptionReconciliation {
    new_subs: Vec<Arc<SubscriptionDataConfig>>,
    removed_subs: Vec<Arc<SubscriptionDataConfig>>,
}

fn settle_fill_event(
    adapter: &PyAlgorithmAdapter,
    portfolio: &Arc<SecurityPortfolioManager>,
    order_processor: &OrderProcessor,
    event: &mut OrderEvent,
) -> Option<Decimal> {
    if !event.is_fill() {
        return None;
    }
    let order = order_processor
        .transaction_manager
        .get_order(event.order_id)?;
    let (fee, contract_multiplier) = {
        let alg = adapter.inner.lock().unwrap();
        (
            alg.order_fee(&order, event.fill_price).amount,
            alg.contract_multiplier_for_symbol(&order.symbol),
        )
    };
    if let Err(message) =
        adapter
            .inner
            .lock()
            .unwrap()
            .validate_order_buying_power(&order, event.fill_price, fee)
    {
        *event = OrderEvent::invalid(order.id, order.symbol.clone(), event.utc_time, message);
        return None;
    }
    portfolio.apply_fill_with_multiplier(
        &order.symbol,
        event.fill_price,
        event.fill_quantity,
        fee,
        contract_multiplier,
    );
    event.order_fee = fee;
    Some(fee)
}

trait OrderEventSidecarWriter {
    fn append_order_events(&self, events: &[OrderEvent]);
    fn append_trades(&self, trades: &[Trade]);
}

fn as_sidecar_writer<T: OrderEventSidecarWriter>(
    writer: Option<&T>,
) -> Option<&dyn OrderEventSidecarWriter> {
    writer.map(|writer| writer as &dyn OrderEventSidecarWriter)
}

struct OrderEventProcessingContext<'a> {
    adapter: &'a mut PyAlgorithmAdapter,
    portfolio: &'a Arc<SecurityPortfolioManager>,
    order_processor: &'a OrderProcessor,
    all_order_events: &'a mut Vec<OrderEvent>,
    trade_builder: &'a mut TradeBuilder,
    completed_trades: &'a mut Vec<Trade>,
    live_writer: Option<&'a dyn OrderEventSidecarWriter>,
}

impl OrderEventProcessingContext<'_> {
    fn process(&mut self, events: &mut [OrderEvent]) {
        for event in events {
            let fee = if event.is_fill() {
                settle_fill_event(&*self.adapter, self.portfolio, self.order_processor, event)
            } else {
                None
            };

            self.order_processor
                .transaction_manager
                .process_order_event(event.clone());

            self.all_order_events.push(event.clone());
            if let Some(writer) = self.live_writer {
                writer.append_order_events(std::slice::from_ref(event));
            }

            if let Some(fee) = fee {
                self.record_trade_fill(event, fee);
            }

            self.adapter.on_order_event(event);
        }
    }

    fn record_trade_fill(&mut self, event: &OrderEvent, fees: Decimal) {
        let Some(trade) = self.trade_builder.record_fill(
            &event.symbol,
            event.utc_time,
            event.fill_price,
            event.fill_quantity,
            SecurityHolding::infer_contract_multiplier(&event.symbol),
            fees,
        ) else {
            return;
        };
        if let Some(writer) = self.live_writer {
            writer.append_trades(std::slice::from_ref(&trade));
        }
        self.completed_trades.push(trade);
    }
}

fn cancel_event_from_order(order: &Order, time: DateTime, message: String) -> OrderEvent {
    let mut event = OrderEvent::new(order.id, order.symbol.clone(), time, OrderStatus::Canceled);
    event.direction = order.direction();
    event.quantity = order.quantity;
    event.limit_price = order.limit_price;
    event.stop_price = order.stop_price;
    event.trailing_amount = order.trailing_amount;
    event.trailing_as_percentage = order.trailing_as_percent;
    event.message = message;
    event
}

fn active_status_after_cancel_rejection(order: &Order) -> OrderStatus {
    if !order.filled_quantity.is_zero() {
        OrderStatus::PartiallyFilled
    } else {
        OrderStatus::Submitted
    }
}

fn update_event_from_order(order: &Order, time: DateTime, message: String) -> OrderEvent {
    let mut event = OrderEvent::new(
        order.id,
        order.symbol.clone(),
        time,
        OrderStatus::UpdateSubmitted,
    );
    event.direction = order.direction();
    event.quantity = order.quantity;
    event.limit_price = order.limit_price;
    event.stop_price = order.stop_price;
    event.trailing_amount = order.trailing_amount;
    event.trailing_as_percentage = order.trailing_as_percent;
    event.message = message;
    event
}

fn submit_event_from_order(order: &Order, time: DateTime, message: String) -> OrderEvent {
    let mut event = OrderEvent::new(order.id, order.symbol.clone(), time, OrderStatus::Submitted);
    event.direction = order.direction();
    event.quantity = order.quantity;
    event.message = message;
    event
}

fn active_status_after_update_rejection(order: &Order) -> OrderStatus {
    if !order.filled_quantity.is_zero() {
        OrderStatus::PartiallyFilled
    } else {
        OrderStatus::Submitted
    }
}

fn drain_local_new_orders(
    order_processor: &OrderProcessor,
    time: DateTime,
    source: &str,
) -> Vec<OrderEvent> {
    order_processor
        .transaction_manager
        .get_open_orders()
        .into_iter()
        .filter(|order| order.status == OrderStatus::New)
        .map(|order| submit_event_from_order(&order, time, format!("{source} accepted order")))
        .collect()
}

fn drain_local_update_requests(
    order_processor: &OrderProcessor,
    time: DateTime,
    source: &str,
) -> Vec<OrderEvent> {
    let requests = order_processor.transaction_manager.get_update_requests();
    let mut events = Vec::new();
    for request in requests {
        let Some(order) = order_processor
            .transaction_manager
            .get_order(request.order_id)
        else {
            order_processor
                .transaction_manager
                .clear_update_request(request.order_id);
            continue;
        };
        if order.status != OrderStatus::UpdateSubmitted {
            order_processor
                .transaction_manager
                .clear_update_request(request.order_id);
            continue;
        }
        order_processor
            .transaction_manager
            .clear_update_request(request.order_id);
        events.push(update_event_from_order(
            &order,
            time,
            format!("{source} updated order"),
        ));
    }
    events
}

fn drain_local_cancel_requests(
    order_processor: &OrderProcessor,
    time: DateTime,
    source: &str,
) -> Vec<OrderEvent> {
    let requests = order_processor.transaction_manager.get_cancel_requests();
    let mut events = Vec::new();
    for request in requests {
        let Some(order) = order_processor
            .transaction_manager
            .get_order(request.order_id)
        else {
            order_processor
                .transaction_manager
                .clear_cancel_request(request.order_id);
            continue;
        };
        if order.status != OrderStatus::CancelPending {
            order_processor
                .transaction_manager
                .clear_cancel_request(request.order_id);
            continue;
        }
        order_processor
            .transaction_manager
            .clear_cancel_request(request.order_id);
        events.push(cancel_event_from_order(
            &order,
            time,
            format!("{source} canceled order"),
        ));
    }
    events
}

struct LiveBrokerageBridge {
    brokerage: Box<dyn Brokerage>,
    submitted_order_ids: HashSet<i64>,
    paper_fills: bool,
}

impl LiveBrokerageBridge {
    fn connect(mut brokerage: Box<dyn Brokerage>, paper_fills: bool) -> Result<Self> {
        brokerage
            .connect()
            .with_context(|| format!("failed to connect brokerage {}", brokerage.name()))?;
        info!(
            "Connected brokerage {} with {} fills",
            brokerage.name(),
            if paper_fills { "paper" } else { "brokerage" }
        );
        Ok(Self {
            brokerage,
            submitted_order_ids: HashSet::new(),
            paper_fills,
        })
    }

    fn submit_new_orders(
        &mut self,
        order_processor: &OrderProcessor,
        time: DateTime,
    ) -> Result<Vec<OrderEvent>> {
        let brokerage_name = self.brokerage.name().to_string();
        let orders = order_processor.transaction_manager.get_open_orders();
        let mut events = Vec::new();

        for order in orders {
            if self.submitted_order_ids.contains(&order.id) || order.status != OrderStatus::New {
                continue;
            }

            let brokerage_ids = self
                .brokerage
                .place_order_with_brokerage_ids(order.clone())
                .with_context(|| {
                    format!(
                        "brokerage {} failed to submit order {} for {}",
                        brokerage_name, order.id, order.symbol
                    )
                })?;
            self.submitted_order_ids.insert(order.id);

            let mut event = if let Some(brokerage_ids) = brokerage_ids {
                if !brokerage_ids.is_empty() {
                    let mut updated_order = order.clone();
                    updated_order.brokerage_id = brokerage_ids;
                    order_processor
                        .transaction_manager
                        .update_order(updated_order);
                }
                let mut event =
                    OrderEvent::new(order.id, order.symbol.clone(), time, OrderStatus::Submitted);
                event.direction = order.direction();
                event.quantity = order.quantity;
                event.message = if self.paper_fills {
                    format!("{brokerage_name} accepted order in paper fill mode")
                } else {
                    format!("{brokerage_name} accepted order")
                };
                event
            } else {
                OrderEvent::invalid(
                    order.id,
                    order.symbol.clone(),
                    time,
                    format!("{brokerage_name} rejected order"),
                )
            };
            event.limit_price = order.limit_price;
            event.stop_price = order.stop_price;
            event.trailing_amount = order.trailing_amount;
            event.trailing_as_percentage = order.trailing_as_percent;
            events.push(event);
        }

        Ok(events)
    }

    fn process_update_requests(
        &mut self,
        order_processor: &OrderProcessor,
        time: DateTime,
    ) -> Result<Vec<OrderEvent>> {
        let brokerage_name = self.brokerage.name().to_string();
        let requests = order_processor.transaction_manager.get_update_requests();
        let mut events = Vec::new();

        for request in requests {
            let Some(order) = order_processor
                .transaction_manager
                .get_order(request.order_id)
            else {
                order_processor
                    .transaction_manager
                    .clear_update_request(request.order_id);
                continue;
            };
            if order.status != OrderStatus::UpdateSubmitted {
                order_processor
                    .transaction_manager
                    .clear_update_request(request.order_id);
                continue;
            }

            let needs_broker_update = self.paper_fills || !order.brokerage_id.is_empty();
            let accepted = if needs_broker_update {
                if !self.brokerage.can_update_order(&order, &request) {
                    false
                } else {
                    self.brokerage.update_order(&order).with_context(|| {
                        format!(
                            "brokerage {} failed to update order {} for {}",
                            brokerage_name, order.id, order.symbol
                        )
                    })?
                }
            } else {
                true
            };

            order_processor
                .transaction_manager
                .clear_update_request(request.order_id);
            if accepted {
                events.push(update_event_from_order(
                    &order,
                    time,
                    if needs_broker_update {
                        format!("{brokerage_name} updated order")
                    } else {
                        format!("{brokerage_name} updated local order")
                    },
                ));
            } else {
                order_processor
                    .transaction_manager
                    .update_order(request.previous_order.clone());
                let mut event = OrderEvent::new(
                    request.previous_order.id,
                    request.previous_order.symbol.clone(),
                    time,
                    active_status_after_update_rejection(&request.previous_order),
                );
                event.direction = request.previous_order.direction();
                event.quantity = request.previous_order.quantity;
                event.message = format!("{brokerage_name} rejected update request");
                events.push(event);
            }
        }

        Ok(events)
    }

    fn process_cancel_requests(
        &mut self,
        order_processor: &OrderProcessor,
        time: DateTime,
    ) -> Result<Vec<OrderEvent>> {
        let brokerage_name = self.brokerage.name().to_string();
        let requests = order_processor.transaction_manager.get_cancel_requests();
        let mut events = Vec::new();

        for request in requests {
            let Some(order) = order_processor
                .transaction_manager
                .get_order(request.order_id)
            else {
                order_processor
                    .transaction_manager
                    .clear_cancel_request(request.order_id);
                continue;
            };
            if order.status != OrderStatus::CancelPending {
                order_processor
                    .transaction_manager
                    .clear_cancel_request(request.order_id);
                continue;
            }

            let needs_broker_cancel = self.paper_fills || !order.brokerage_id.is_empty();
            let accepted = if needs_broker_cancel {
                self.brokerage.cancel_order(&order).with_context(|| {
                    format!(
                        "brokerage {} failed to cancel order {} for {}",
                        brokerage_name, order.id, order.symbol
                    )
                })?
            } else {
                true
            };

            order_processor
                .transaction_manager
                .clear_cancel_request(request.order_id);
            if accepted {
                events.push(cancel_event_from_order(
                    &order,
                    time,
                    if needs_broker_cancel {
                        format!("{brokerage_name} canceled order")
                    } else {
                        format!("{brokerage_name} canceled local order")
                    },
                ));
            } else {
                let mut event = OrderEvent::new(
                    order.id,
                    order.symbol.clone(),
                    time,
                    active_status_after_cancel_rejection(&order),
                );
                event.direction = order.direction();
                event.quantity = order.quantity;
                event.message = format!("{brokerage_name} rejected cancel request");
                events.push(event);
            }
        }

        Ok(events)
    }

    fn sync_account_state(&self) -> Result<AccountState> {
        AccountSynchronizer::new(0)
            .sync_blocking(self.brokerage.as_ref())
            .context("failed to synchronize live brokerage account state")
    }

    fn reconcile_order_events(
        &mut self,
        order_processor: &OrderProcessor,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        if self.paper_fills {
            return Vec::new();
        }

        let brokerage_name = self.brokerage.name().to_string();
        let local_orders = order_processor.transaction_manager.get_all_orders();
        let mut local_by_brokerage_id = HashMap::new();
        for order in local_orders {
            for brokerage_id in &order.brokerage_id {
                local_by_brokerage_id.insert(brokerage_id.clone(), order.clone());
            }
        }

        let mut events = Vec::new();
        for brokerage_order in self.brokerage.get_account_orders() {
            let Some(local_order) = brokerage_order
                .brokerage_id
                .iter()
                .find_map(|brokerage_id| local_by_brokerage_id.get(brokerage_id))
            else {
                continue;
            };
            if let Some(event) =
                brokerage_snapshot_event(local_order, &brokerage_order, time, &brokerage_name)
            {
                events.push(event);
            }
        }
        events
    }
}

impl Drop for LiveBrokerageBridge {
    fn drop(&mut self) {
        self.brokerage.disconnect();
    }
}

fn brokerage_snapshot_event(
    local_order: &Order,
    brokerage_order: &Order,
    time: DateTime,
    brokerage_name: &str,
) -> Option<OrderEvent> {
    if local_order.status.is_closed() {
        return None;
    }

    let fill_delta = brokerage_order.filled_quantity - local_order.filled_quantity;
    if !fill_delta.is_zero() {
        let status = if brokerage_order.status == OrderStatus::Filled {
            OrderStatus::Filled
        } else {
            OrderStatus::PartiallyFilled
        };
        let mut event = OrderEvent::new(local_order.id, local_order.symbol.clone(), time, status);
        event.direction = local_order.direction();
        event.quantity = local_order.quantity;
        event.fill_quantity = fill_delta;
        event.fill_price = brokerage_order_fill_price(brokerage_order);
        event.message = format!(
            "{brokerage_name} reported fill for brokerage order {}",
            brokerage_order
                .brokerage_id
                .first()
                .map(String::as_str)
                .unwrap_or("?")
        );
        event.limit_price = local_order.limit_price;
        event.stop_price = local_order.stop_price;
        event.trailing_amount = local_order.trailing_amount;
        event.trailing_as_percentage = local_order.trailing_as_percent;
        return Some(event);
    }

    if brokerage_order.status != local_order.status && brokerage_order.status.is_closed() {
        let mut event = OrderEvent::new(
            local_order.id,
            local_order.symbol.clone(),
            time,
            brokerage_order.status,
        );
        event.direction = local_order.direction();
        event.quantity = local_order.quantity;
        event.message = format!(
            "{brokerage_name} reported {:?} for brokerage order {}",
            brokerage_order.status,
            brokerage_order
                .brokerage_id
                .first()
                .map(String::as_str)
                .unwrap_or("?")
        );
        event.limit_price = local_order.limit_price;
        event.stop_price = local_order.stop_price;
        event.trailing_amount = local_order.trailing_amount;
        event.trailing_as_percentage = local_order.trailing_as_percent;
        return Some(event);
    }

    None
}

fn brokerage_order_fill_price(order: &Order) -> Decimal {
    if order.average_fill_price > Decimal::ZERO {
        order.average_fill_price
    } else if order.price > Decimal::ZERO {
        order.price
    } else {
        order
            .limit_price
            .or(order.stop_price)
            .unwrap_or(Decimal::ZERO)
    }
}

fn apply_initial_brokerage_account_state(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    account_state: &AccountState,
) {
    let mut algorithm = algorithm.lock().unwrap();
    if account_state
        .cash_balances
        .iter()
        .any(|(currency, _)| currency.eq_ignore_ascii_case("USD"))
    {
        *algorithm.portfolio.cash.write() = account_state.cash;
    }

    for holding in &account_state.holdings {
        if !algorithm.securities.contains(&holding.symbol) {
            algorithm.add_security_symbol(holding.symbol.clone(), Resolution::Minute);
        }
        let multiplier = algorithm.contract_multiplier_for_symbol(&holding.symbol);
        algorithm.portfolio.set_holdings(
            &holding.symbol,
            holding.average_price,
            holding.quantity,
            multiplier,
        );
    }

    for order in &account_state.open_orders {
        algorithm.transactions.add_or_update_order(order.clone());
    }
}

fn reconcile_runner_subscriptions(
    subscriptions: &mut Vec<Arc<SubscriptionDataConfig>>,
    loaded_subscription_ids: &mut HashSet<u64>,
    current_subs: &[Arc<SubscriptionDataConfig>],
) -> SubscriptionReconciliation {
    let active_ids: HashSet<u64> = current_subs.iter().map(|sub| sub.unique_id()).collect();
    let mut removed_subs = Vec::new();

    subscriptions.retain(|sub| {
        let is_active = active_ids.contains(&sub.unique_id());
        if !is_active {
            removed_subs.push(sub.clone());
        }
        is_active
    });
    loaded_subscription_ids.retain(|id| active_ids.contains(id));

    let mut new_subs = Vec::new();
    for sub in current_subs {
        if loaded_subscription_ids.insert(sub.unique_id()) {
            subscriptions.push(sub.clone());
            new_subs.push(sub.clone());
        }
    }

    SubscriptionReconciliation {
        new_subs,
        removed_subs,
    }
}

pub struct RunConfig {
    pub data_root: PathBuf,
    pub _compression_level: i32,
    /// If set, missing price data is fetched from this provider before the backtest loop.
    pub historical_provider: Option<Arc<dyn IHistoricalDataProvider>>,
    /// Raw stacked provider for DataType-specific requests (e.g. FactorFile).
    /// Providers that don't support a DataType return NotImplemented: and the
    /// next provider in the stack is tried.
    pub history_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    /// Override the strategy's set_start_date (YYYY-MM-DD).
    pub start_date_override: Option<chrono::NaiveDate>,
    /// Override the strategy's set_end_date (YYYY-MM-DD).
    pub end_date_override: Option<chrono::NaiveDate>,
    /// Algorithm parameters available through QCAlgorithm.get_parameter().
    pub parameters: HashMap<String, String>,
    /// Custom data source plugins loaded from `~/.rlean/plugins/` or set explicitly.
    /// Keyed by `source_type` name (e.g. `"fred"`, `"cboe_vix"`).
    pub custom_data_sources: Vec<Arc<dyn lean_data_providers::ICustomDataSource>>,
    /// Optional backtest output directory. When set, progress/order/trade sidecar
    /// files are written while the backtest is still running.
    pub output_dir: Option<PathBuf>,
}

impl Default for RunConfig {
    fn default() -> Self {
        RunConfig {
            data_root: PathBuf::from("data"),
            _compression_level: 3,
            historical_provider: None,
            history_provider: None,
            start_date_override: None,
            end_date_override: None,
            parameters: HashMap::new(),
            custom_data_sources: vec![],
            output_dir: None,
        }
    }
}

pub struct LiveRunConfig {
    pub data_root: PathBuf,
    pub history_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    pub parameters: HashMap<String, String>,
    pub custom_data_sources: Vec<Arc<dyn lean_data_providers::ICustomDataSource>>,
    pub live_data_queue: DataQueueHandlerManager,
    /// Optional real brokerage adapter. With `paper_trading=true`, the
    /// brokerage is connected and synced while orders are locally acknowledged
    /// and filled by the paper fill model. With `paper_trading=false`, new
    /// orders are submitted to the brokerage before fill events are processed.
    pub brokerage: Option<Box<dyn Brokerage>>,
    /// Optional brokerage model selected by the live CLI. This is applied
    /// before Initialize(), so user code can still override it explicitly.
    pub brokerage_model: Option<BrokerageName>,
    pub paper_trading: bool,
    /// Stops after this many emitted slices. Intended for integration tests and
    /// smoke runs; `None` runs until every live subscription closes.
    pub max_slices: Option<usize>,
    /// Stops the live run after this wall-clock duration. Intended for paper
    /// deployment soaks and integration tests.
    pub max_runtime: Option<Duration>,
    /// Optional live deployment directory. When set, live portfolio/order/log
    /// sidecars are written while the run is still active.
    pub output_dir: Option<PathBuf>,
}

pub struct LiveRunResult {
    pub slices_processed: usize,
    pub final_value: f64,
    pub order_events: Vec<OrderEvent>,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub stopped_at: chrono::DateTime<chrono::Utc>,
}

pub struct BacktestResult {
    pub trading_days: i64,
    pub final_value: f64,
    pub total_return: f64,
    pub starting_cash: f64,
    pub start_date: chrono::NaiveDate,
    pub end_date: chrono::NaiveDate,
    /// Daily portfolio values (one per trading day, in order).
    pub equity_curve: Vec<f64>,
    /// ISO date strings matching equity_curve.
    pub daily_dates: Vec<String>,
    /// Daily benchmark prices, in order.
    pub benchmark_curve: Vec<f64>,
    /// ISO date strings matching benchmark_curve.
    pub benchmark_dates: Vec<String>,
    /// Full statistics computed at the end of the backtest.
    pub statistics: PortfolioStatistics,
    /// Custom strategy charts plotted via self.plot().
    pub charts: ChartCollection,
    /// All order fill events from the backtest run.
    pub order_events: Vec<OrderEvent>,
    /// Symbols/dates for which data was found in the Parquet store.
    pub succeeded_data_requests: Vec<String>,
    /// Symbols/dates for which no data was found.
    pub failed_data_requests: Vec<String>,
    /// Unix epoch seconds at backtest start (used as backtest ID).
    pub backtest_id: i64,
    /// The ticker used as the benchmark (e.g. "SPY").
    pub benchmark_symbol: String,
}

struct LiveBacktestWriter {
    dir: PathBuf,
    progress_path: PathBuf,
    order_events_path: PathBuf,
    trades_path: PathBuf,
    heartbeat_path: PathBuf,
    start_date: NaiveDate,
    end_date: NaiveDate,
    started_at: chrono::DateTime<chrono::Utc>,
    last_log_date: Option<NaiveDate>,
    last_heartbeat: Instant,
}

impl LiveBacktestWriter {
    fn new(dir: PathBuf, start_date: NaiveDate, end_date: NaiveDate) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let writer = LiveBacktestWriter {
            progress_path: dir.join("progress.json"),
            order_events_path: dir.join("order-events.jsonl"),
            trades_path: dir.join("trades.jsonl"),
            heartbeat_path: dir.join("heartbeat.log"),
            dir,
            start_date,
            end_date,
            started_at: chrono::Utc::now(),
            last_log_date: None,
            last_heartbeat: Instant::now() - Duration::from_secs(60),
        };
        let _ = std::fs::File::create(&writer.order_events_path);
        let _ = std::fs::File::create(&writer.trades_path);
        let _ = std::fs::File::create(&writer.heartbeat_path);
        writer
    }

    fn progress_fraction(&self, current_date: NaiveDate) -> f64 {
        let total = (self.end_date - self.start_date).num_days().max(1) as f64;
        let done = (current_date - self.start_date).num_days().max(0) as f64;
        (done / total).clamp(0.0, 1.0)
    }

    fn record_progress(
        &mut self,
        current_date: NaiveDate,
        trading_days: i64,
        portfolio_value: Decimal,
        order_events: usize,
        trades: usize,
    ) {
        let progress = self.progress_fraction(current_date);
        let payload = serde_json::json!({
            "status": "running",
            "current_date": current_date.to_string(),
            "start_date": self.start_date.to_string(),
            "end_date": self.end_date.to_string(),
            "progress": progress,
            "progress_percent": (progress * 100.0),
            "trading_days": trading_days,
            "portfolio_value": portfolio_value.to_string(),
            "order_events": order_events,
            "trades": trades,
            "started_at": self.started_at.to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        let tmp = self.progress_path.with_extension("json.tmp");
        if let Ok(json) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(&tmp, json);
            let _ = std::fs::rename(&tmp, &self.progress_path);
        }

        if self.last_log_date != Some(current_date) {
            info!(
                "Backtest progress: {} ({:.1}%) trading_days={} portfolio={} orders={} trades={} output={}",
                current_date,
                progress * 100.0,
                trading_days,
                portfolio_value,
                order_events,
                trades,
                self.dir.display()
            );
            self.last_log_date = Some(current_date);
        }

        if self.last_heartbeat.elapsed() >= Duration::from_secs(30) {
            self.append_heartbeat(current_date, progress, trading_days, portfolio_value);
            self.last_heartbeat = Instant::now();
        }
    }

    fn append_order_events(&self, events: &[OrderEvent]) {
        append_json_lines(&self.order_events_path, events);
    }

    fn append_trades(&self, trades: &[Trade]) {
        append_json_lines(&self.trades_path, trades);
    }

    fn append_heartbeat(
        &self,
        current_date: NaiveDate,
        progress: f64,
        trading_days: i64,
        portfolio_value: Decimal,
    ) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.heartbeat_path)
        {
            let _ = writeln!(
                file,
                "{} current_date={} progress={:.3} trading_days={} portfolio={}",
                chrono::Utc::now().to_rfc3339(),
                current_date,
                progress,
                trading_days,
                portfolio_value
            );
        }
    }

    fn mark_completed(
        &self,
        trading_days: i64,
        portfolio_value: Decimal,
        order_events: usize,
        trades: usize,
    ) {
        let payload = serde_json::json!({
            "status": "completed",
            "current_date": self.end_date.to_string(),
            "start_date": self.start_date.to_string(),
            "end_date": self.end_date.to_string(),
            "progress": 1.0,
            "progress_percent": 100.0,
            "trading_days": trading_days,
            "portfolio_value": portfolio_value.to_string(),
            "order_events": order_events,
            "trades": trades,
            "started_at": self.started_at.to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        if let Ok(json) = serde_json::to_string_pretty(&payload) {
            let _ = std::fs::write(&self.progress_path, json);
        }
    }
}

impl OrderEventSidecarWriter for LiveBacktestWriter {
    fn append_order_events(&self, events: &[OrderEvent]) {
        LiveBacktestWriter::append_order_events(self, events);
    }

    fn append_trades(&self, trades: &[Trade]) {
        LiveBacktestWriter::append_trades(self, trades);
    }
}

struct LiveDeploymentWriter {
    dir: PathBuf,
    progress_path: PathBuf,
    portfolio_path: PathBuf,
    orders_path: PathBuf,
    order_events_path: PathBuf,
    trades_path: PathBuf,
    heartbeat_path: PathBuf,
    started_at: chrono::DateTime<chrono::Utc>,
    last_heartbeat: std::sync::Mutex<Instant>,
}

impl LiveDeploymentWriter {
    fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        let writer = Self {
            progress_path: dir.join("progress.json"),
            portfolio_path: dir.join("portfolio.json"),
            orders_path: dir.join("orders.json"),
            order_events_path: dir.join("order-events.jsonl"),
            trades_path: dir.join("trades.jsonl"),
            heartbeat_path: dir.join("heartbeat.log"),
            dir,
            started_at: chrono::Utc::now(),
            last_heartbeat: std::sync::Mutex::new(Instant::now() - Duration::from_secs(60)),
        };
        let _ = std::fs::File::create(&writer.order_events_path);
        let _ = std::fs::File::create(&writer.trades_path);
        let _ = std::fs::File::create(&writer.heartbeat_path);
        writer
    }

    fn append_order_events(&self, events: &[OrderEvent]) {
        append_json_lines(&self.order_events_path, events);
    }

    fn append_trades(&self, trades: &[Trade]) {
        append_json_lines(&self.trades_path, trades);
    }

    fn record_snapshot(
        &self,
        time: DateTime,
        portfolio: &SecurityPortfolioManager,
        order_processor: &OrderProcessor,
        slices_processed: usize,
        order_events: usize,
        trades: usize,
    ) {
        let updated_at = chrono::Utc::now().to_rfc3339();
        let holdings = portfolio
            .all_holdings()
            .into_iter()
            .map(|holding| {
                serde_json::json!({
                    "symbol": holding.symbol.value,
                    "sid": holding.symbol.id.sid,
                    "market": holding.symbol.id.market.as_str(),
                    "security_type": holding.symbol.security_type().to_string(),
                    "quantity": holding.quantity.to_string(),
                    "average_price": holding.average_price.to_string(),
                    "last_price": holding.last_price.to_string(),
                    "market_value": holding.market_value().to_string(),
                    "portfolio_value_contribution": holding.portfolio_value_contribution().to_string(),
                    "unrealized_pnl": holding.unrealized_pnl.to_string(),
                    "realized_pnl": holding.realized_pnl.to_string(),
                    "total_fees": holding.total_fees.to_string(),
                    "contract_multiplier": holding.contract_multiplier.to_string(),
                    "invested": holding.is_invested(),
                })
            })
            .collect::<Vec<_>>();

        let portfolio_payload = serde_json::json!({
            "status": "running",
            "time": time.to_string(),
            "updated_at": updated_at,
            "started_at": self.started_at.to_rfc3339(),
            "cash": portfolio.cash.read().to_string(),
            "starting_cash": portfolio.starting_cash.to_string(),
            "total_portfolio_value": portfolio.total_portfolio_value().to_string(),
            "total_holdings_value": portfolio.total_holdings_value().to_string(),
            "unrealized_pnl": portfolio.unrealized_profit().to_string(),
            "total_return": portfolio.total_return_pct().to_string(),
            "total_fees": portfolio.total_fees.read().to_string(),
            "holdings": holdings,
        });
        write_json_pretty_atomic(&self.portfolio_path, &portfolio_payload);

        let orders = order_processor.transaction_manager.get_all_orders();
        let orders_payload = serde_json::json!({
            "status": "running",
            "time": time.to_string(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "count": orders.len(),
            "orders": orders,
        });
        write_json_pretty_atomic(&self.orders_path, &orders_payload);

        let progress_payload = serde_json::json!({
            "status": "running",
            "time": time.to_string(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "started_at": self.started_at.to_rfc3339(),
            "slices_processed": slices_processed,
            "portfolio_value": portfolio.total_portfolio_value().to_string(),
            "order_events": order_events,
            "trades": trades,
            "output": self.dir,
        });
        write_json_pretty_atomic(&self.progress_path, &progress_payload);
        self.append_heartbeat(time, portfolio, slices_processed, order_events, trades);
    }

    fn append_heartbeat(
        &self,
        time: DateTime,
        portfolio: &SecurityPortfolioManager,
        slices_processed: usize,
        order_events: usize,
        trades: usize,
    ) {
        let Ok(mut last_heartbeat) = self.last_heartbeat.lock() else {
            return;
        };
        if last_heartbeat.elapsed() < Duration::from_secs(30) {
            return;
        }
        *last_heartbeat = Instant::now();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.heartbeat_path)
        {
            let _ = writeln!(
                file,
                "{} time={} slices={} portfolio={} order_events={} trades={}",
                chrono::Utc::now().to_rfc3339(),
                time,
                slices_processed,
                portfolio.total_portfolio_value(),
                order_events,
                trades
            );
        }
    }

    fn mark_stopped(
        &self,
        status: &str,
        portfolio: &SecurityPortfolioManager,
        slices_processed: usize,
        order_events: usize,
        trades: usize,
    ) {
        let payload = serde_json::json!({
            "status": status,
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "started_at": self.started_at.to_rfc3339(),
            "stopped_at": chrono::Utc::now().to_rfc3339(),
            "slices_processed": slices_processed,
            "portfolio_value": portfolio.total_portfolio_value().to_string(),
            "order_events": order_events,
            "trades": trades,
            "output": self.dir,
        });
        write_json_pretty_atomic(&self.progress_path, &payload);
    }
}

impl OrderEventSidecarWriter for LiveDeploymentWriter {
    fn append_order_events(&self, events: &[OrderEvent]) {
        LiveDeploymentWriter::append_order_events(self, events);
    }

    fn append_trades(&self, trades: &[Trade]) {
        LiveDeploymentWriter::append_trades(self, trades);
    }
}

fn write_json_pretty_atomic<T: serde::Serialize>(path: &Path, value: &T) {
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(&tmp, json);
        let _ = std::fs::rename(&tmp, path);
    }
}

fn append_json_lines<T: serde::Serialize>(path: &Path, values: &[T]) {
    if values.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        for value in values {
            if let Ok(line) = serde_json::to_string(value) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}

impl BacktestResult {
    pub fn print_summary(&self) {
        use rust_decimal::prelude::ToPrimitive;
        let s = &self.statistics;
        println!("╔══════════════════════════════════════════════════════╗");
        println!("║                  Backtest Complete                   ║");
        println!("╠══════════════════════════════════════════════════════╣");
        let row = |label: &str, value: &str| {
            println!("║  {:<30} {:>20}  ║", label, value);
        };
        row("Start Date", &self.start_date.to_string());
        row("End Date", &self.end_date.to_string());
        row("Trading Days", &self.trading_days.to_string());
        row("Starting Cash", &format!("${:.2}", self.starting_cash));
        row("Final Value", &format!("${:.2}", self.final_value));
        row(
            "Total Return",
            &format!("{:.2}%", self.total_return * 100.0),
        );
        row(
            "CAGR",
            &format!(
                "{:.2}%",
                s.compounding_annual_return.to_f64().unwrap_or(0.0) * 100.0
            ),
        );
        row(
            "Sharpe Ratio",
            &format!("{:.3}", s.sharpe_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Sortino Ratio",
            &format!("{:.3}", s.sortino_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Probabilistic SR",
            &format!(
                "{:.1}%",
                s.probabilistic_sharpe_ratio.to_f64().unwrap_or(0.0) * 100.0
            ),
        );
        row(
            "Calmar Ratio",
            &format!("{:.3}", s.calmar_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Omega Ratio",
            &format!("{:.3}", s.omega_ratio.to_f64().unwrap_or(0.0)),
        );
        row(
            "Max Drawdown",
            &format!("{:.2}%", s.drawdown.to_f64().unwrap_or(0.0) * 100.0),
        );
        row(
            "Recovery Factor",
            &format!("{:.2}", s.recovery_factor.to_f64().unwrap_or(0.0)),
        );
        row(
            "Annual Std Dev",
            &format!(
                "{:.2}%",
                s.annual_standard_deviation.to_f64().unwrap_or(0.0) * 100.0
            ),
        );
        row(
            "Alpha",
            &format!("{:.2}%", s.alpha.to_f64().unwrap_or(0.0) * 100.0),
        );
        row("Beta", &format!("{:.3}", s.beta.to_f64().unwrap_or(0.0)));
        row(
            "Treynor Ratio",
            &format!("{:.3}", s.treynor_ratio.to_f64().unwrap_or(0.0)),
        );
        println!("╚══════════════════════════════════════════════════════╝");
    }
}

/// Load a Python strategy file, find the `QcAlgorithm` subclass,
/// instantiate it, and return a `PyAlgorithmAdapter` ready to run.
pub fn load_strategy(py: Python<'_>, strategy_path: &Path) -> Result<PyAlgorithmAdapter> {
    // Add the strategy directory to sys.path.
    let parent = strategy_path.parent().unwrap_or(Path::new("."));
    let sys = py.import("sys").context("failed to import sys")?;
    let path_list = sys.getattr("path").context("no sys.path")?;
    if let Some(site_packages) = rlean_python_site_packages(py)? {
        path_list
            .call_method1("insert", (0, site_packages.to_string_lossy().as_ref()))
            .context("failed to insert rlean Python site-packages to sys.path")?;
    }
    path_list
        .call_method1("insert", (0, parent.to_string_lossy().as_ref()))
        .context("failed to insert to sys.path")?;

    // Read and compile the strategy source.
    let code_str = std::fs::read_to_string(strategy_path)
        .with_context(|| format!("cannot read {}", strategy_path.display()))?;
    let filename_str = strategy_path.to_string_lossy().to_string();

    // pyo3 0.23 requires &CStr
    use std::ffi::CString;
    let code_c = CString::new(code_str.as_str()).context("strategy code contains null byte")?;
    let filename_c = CString::new(filename_str.as_str()).context("filename contains null byte")?;
    let modname_c = CString::new("strategy").unwrap();

    let module = PyModule::from_code(py, &code_c, &filename_c, &modname_c)
        .with_context(|| format!("failed to compile {}", strategy_path.display()))?;

    // Get the QCAlgorithm base class from the AlgorithmImports module.
    // Try AlgorithmImports first (new name), fall back to lean_rust (old name).
    let lean_mod = py.import("AlgorithmImports")
        .or_else(|_| py.import("lean_rust"))
        .context("AlgorithmImports not importable — was append_to_inittab!(lean_python::AlgorithmImports) called before Python::initialize()?")?;
    let base_class = lean_mod
        .getattr("QCAlgorithm")
        .or_else(|_| lean_mod.getattr("QcAlgorithm"))
        .context("QCAlgorithm not found in AlgorithmImports")?;

    // Walk the module namespace to find the first QcAlgorithm subclass.
    let builtins = py.import("builtins")?;
    let issubclass_fn = builtins.getattr("issubclass")?;

    let mut strategy_class: Option<Bound<'_, PyAny>> = None;
    for (_, value) in module.dict() {
        if !value.is_instance_of::<PyType>() {
            continue;
        }
        if value.eq(&base_class).unwrap_or(false) {
            continue;
        }

        let is_sub = issubclass_fn
            .call1((&value, &base_class))
            .and_then(|r| r.extract::<bool>())
            .unwrap_or(false);

        if is_sub {
            let name = value
                .getattr("__name__")
                .map(|n| n.to_string())
                .unwrap_or_default();
            info!("Found strategy class: {}", name);
            strategy_class = Some(value);
            break;
        }
    }

    let cls = strategy_class.ok_or_else(|| {
        anyhow::anyhow!(
            "No QcAlgorithm subclass found in {}",
            strategy_path.display()
        )
    })?;

    let instance = cls
        .call0()
        .context("failed to instantiate strategy class")?;
    let instance_py = instance.unbind();

    PyAlgorithmAdapter::from_instance(py, instance_py)
        .context("strategy class must inherit from AlgorithmImports.QCAlgorithm")
}

fn rlean_python_site_packages(py: Python<'_>) -> Result<Option<PathBuf>> {
    let home = match std::env::var("HOME") {
        Ok(home) => home,
        Err(_) => return Ok(None),
    };
    let sys = py.import("sys").context("failed to import sys")?;
    let version_info = sys
        .getattr("version_info")
        .context("failed to read sys.version_info")?;
    let major: u8 = version_info.getattr("major")?.extract()?;
    let minor: u8 = version_info.getattr("minor")?.extract()?;
    let site_packages = PathBuf::from(home)
        .join(".rlean")
        .join("python")
        .join(format!("cp{major}{minor}"))
        .join("site-packages");
    Ok(site_packages.exists().then_some(site_packages))
}

struct ActiveLiveSubscription {
    key: LiveSubscriptionKey,
    receiver: crossbeam_channel::Receiver<lean_core::Result<LiveDataItem>>,
}

fn add_active_live_subscription(
    active: &mut Vec<ActiveLiveSubscription>,
    subscription: LiveDataSubscription,
) {
    active.push(ActiveLiveSubscription {
        key: subscription.key(),
        receiver: subscription.receiver,
    });
}

fn remove_active_live_subscription(
    active: &mut Vec<ActiveLiveSubscription>,
    key: &LiveSubscriptionKey,
) {
    active.retain(|subscription| &subscription.key != key);
}

fn collect_live_universe_subscriptions(
    adapter: &PyAlgorithmAdapter,
) -> Vec<LiveUniverseSubscriptionConfig> {
    Python::attach(|py| {
        let universes = adapter.universes.lock().unwrap();
        universes
            .iter()
            .filter_map(|universe| universe.bind(py).borrow().live_universe_subscription())
            .collect()
    })
}

fn custom_data_source_for(
    custom_data_sources: &[Arc<dyn lean_data_providers::ICustomDataSource>],
    source_type: &str,
) -> Option<Arc<dyn lean_data_providers::ICustomDataSource>> {
    custom_data_sources
        .iter()
        .find(|source| source.name().eq_ignore_ascii_case(source_type))
        .cloned()
}

fn subscribe_live_custom_data(
    live_data_queue: &mut DataQueueHandlerManager,
    custom_data_sources: &[Arc<dyn lean_data_providers::ICustomDataSource>],
    data_root: PathBuf,
    subscription: &CustomDataSubscription,
) -> Result<LiveDataSubscription> {
    match live_data_queue.subscribe_custom(subscription) {
        Ok(subscription) => Ok(subscription),
        Err(LeanError::Unsupported(reason)) => subscribe_custom_data_source_live(
            custom_data_sources,
            data_root,
            subscription.clone(),
            reason,
        ),
        Err(error) => Err(anyhow::anyhow!(error.to_string())),
    }
}

fn subscribe_live_universe_data(
    live_data_queue: &mut DataQueueHandlerManager,
    custom_data_sources: &[Arc<dyn lean_data_providers::ICustomDataSource>],
    data_root: PathBuf,
    subscription: &LiveUniverseSubscriptionConfig,
) -> Result<LiveDataSubscription> {
    match live_data_queue.subscribe_universe(subscription) {
        Ok(subscription) => Ok(subscription),
        Err(LeanError::Unsupported(reason)) => subscribe_universe_data_source_live(
            custom_data_sources,
            data_root,
            subscription.clone(),
            reason,
        ),
        Err(error) => Err(anyhow::anyhow!(error.to_string())),
    }
}

fn subscribe_custom_data_source_live(
    custom_data_sources: &[Arc<dyn lean_data_providers::ICustomDataSource>],
    data_root: PathBuf,
    subscription: CustomDataSubscription,
    unsupported_reason: String,
) -> Result<LiveDataSubscription> {
    let source = custom_data_source_for(custom_data_sources, &subscription.source_type)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no live data queue handler or custom data source supports {}:{}; tried: {}",
                subscription.source_type,
                subscription.ticker,
                unsupported_reason
            )
        })?;
    let (sender, receiver) = live_data_channel();
    let live_subscription = LiveDataSubscription::new(
        LiveDataSubscriptionConfig::Custom(subscription.clone()),
        receiver,
    );
    tokio::spawn(poll_custom_data_source_subscription(
        data_root,
        source,
        subscription,
        sender,
    ));
    Ok(live_subscription)
}

fn subscribe_universe_data_source_live(
    custom_data_sources: &[Arc<dyn lean_data_providers::ICustomDataSource>],
    data_root: PathBuf,
    subscription: LiveUniverseSubscriptionConfig,
    unsupported_reason: String,
) -> Result<LiveDataSubscription> {
    let source = custom_data_source_for(custom_data_sources, &subscription.source_type)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no live data queue handler or custom data source supports universe {}:{}; tried: {}",
                subscription.source_type,
                subscription.ticker,
                unsupported_reason
            )
        })?;
    let (sender, receiver) = live_data_channel();
    let live_subscription = LiveDataSubscription::new(
        LiveDataSubscriptionConfig::Universe(subscription.clone()),
        receiver,
    );
    tokio::spawn(poll_universe_data_source_subscription(
        data_root,
        source,
        subscription,
        sender,
    ));
    Ok(live_subscription)
}

async fn poll_custom_data_source_subscription(
    data_root: PathBuf,
    source: Arc<dyn lean_data_providers::ICustomDataSource>,
    subscription: CustomDataSubscription,
    sender: crossbeam_channel::Sender<lean_core::Result<LiveDataItem>>,
) {
    let mut seen = HashSet::new();
    loop {
        let utc_time = DateTime::now();
        let dynamic_query = subscription.dynamic_query.clone();

        let load_result = match load_live_custom_data_points_for_subscription_with_status(
            data_root.clone(),
            subscription.source_type.clone(),
            subscription.ticker.clone(),
            utc_time,
            Arc::clone(&source),
            subscription.config.clone(),
            dynamic_query.clone(),
        )
        .await
        {
            Ok(load_result) => {
                let source_available = load_result.source_available;
                for point in load_result.points {
                    if !seen.insert(custom_data_point_key(&point)) {
                        continue;
                    }
                    if sender
                        .send(Ok(LiveDataItem::CustomData {
                            source_type: subscription.source_type.clone(),
                            ticker: subscription.ticker.clone(),
                            point,
                        }))
                        .is_err()
                    {
                        return;
                    }
                }
                CustomDataLoadResult {
                    points: Vec::new(),
                    source_available,
                }
            }
            Err(error) => {
                if sender
                    .send(Err(lean_core::LeanError::DataError(format!(
                        "live custom data poll {}:{} failed: {error:#}",
                        subscription.source_type, subscription.ticker
                    ))))
                    .is_err()
                {
                    return;
                }
                CustomDataLoadResult {
                    points: Vec::new(),
                    source_available: false,
                }
            }
        };
        let sleep = source.live_poll_delay(
            &subscription.ticker,
            utc_time,
            load_result.source_available,
            &subscription.config,
            &dynamic_query,
        );
        tokio::time::sleep(sleep).await;
    }
}

async fn poll_universe_data_source_subscription(
    data_root: PathBuf,
    source: Arc<dyn lean_data_providers::ICustomDataSource>,
    subscription: LiveUniverseSubscriptionConfig,
    sender: crossbeam_channel::Sender<lean_core::Result<LiveDataItem>>,
) {
    let mut seen_batches = HashSet::new();
    loop {
        let utc_time = DateTime::now();
        let dynamic_query = CustomDataQuery::default();
        let config = CustomDataConfig {
            ticker: subscription.ticker.clone(),
            source_type: subscription.source_type.clone(),
            resolution: subscription.resolution,
            properties: subscription.properties.clone(),
            query: Default::default(),
        };
        let load_result = match load_live_custom_data_points_for_subscription_with_status(
            data_root.clone(),
            subscription.source_type.clone(),
            subscription.ticker.clone(),
            utc_time,
            Arc::clone(&source),
            config.clone(),
            dynamic_query.clone(),
        )
        .await
        {
            Ok(load_result) => {
                let new_points = load_result
                    .points
                    .into_iter()
                    .filter(|point| seen_batches.insert(custom_data_point_key(point)))
                    .collect::<Vec<_>>();
                if !new_points.is_empty() {
                    let time = new_points
                        .iter()
                        .filter_map(|point| point.end_time)
                        .max_by_key(|time| time.0)
                        .unwrap_or_else(DateTime::now);
                    if sender
                        .send(Ok(LiveDataItem::UniverseData {
                            source_type: subscription.source_type.clone(),
                            ticker: subscription.ticker.clone(),
                            resolution: subscription.resolution,
                            time,
                            data: new_points,
                        }))
                        .is_err()
                    {
                        return;
                    }
                }
                CustomDataLoadResult {
                    points: Vec::new(),
                    source_available: load_result.source_available,
                }
            }
            Err(error) => {
                if sender
                    .send(Err(lean_core::LeanError::DataError(format!(
                        "live universe data poll {}:{} failed: {error:#}",
                        subscription.source_type, subscription.ticker
                    ))))
                    .is_err()
                {
                    return;
                }
                CustomDataLoadResult {
                    points: Vec::new(),
                    source_available: false,
                }
            }
        };
        let sleep = source.live_poll_delay(
            &subscription.ticker,
            utc_time,
            load_result.source_available,
            &config,
            &dynamic_query,
        );
        tokio::time::sleep(sleep).await;
    }
}

fn custom_data_point_key(point: &CustomDataPoint) -> String {
    let time = point
        .end_time
        .map(|time| time.0.to_string())
        .unwrap_or_else(|| point.time.to_string());
    let fields = serde_json::to_string(&point.fields).unwrap_or_default();
    format!("{time}:{}:{fields}", point.value)
}

fn reconcile_live_market_subscriptions(
    adapter: &PyAlgorithmAdapter,
    subscriptions: &mut Vec<Arc<SubscriptionDataConfig>>,
    loaded_subscription_ids: &mut HashSet<u64>,
    active: &mut Vec<ActiveLiveSubscription>,
    live_data_queue: &mut DataQueueHandlerManager,
) -> Result<bool> {
    let current_subs = adapter.inner.lock().unwrap().subscription_manager.get_all();
    let reconciliation =
        reconcile_runner_subscriptions(subscriptions, loaded_subscription_ids, &current_subs);

    let changed = !reconciliation.new_subs.is_empty() || !reconciliation.removed_subs.is_empty();

    for removed in reconciliation.removed_subs {
        live_data_queue.unsubscribe(removed.as_ref())?;
        remove_active_live_subscription(active, &LiveSubscriptionKey::Market(removed.unique_id()));
    }

    for new_sub in reconciliation.new_subs {
        let subscription = live_data_queue.subscribe(new_sub.as_ref())?;
        add_active_live_subscription(active, subscription);
    }

    Ok(changed)
}

fn live_prices_from_slice(slice: &Slice) -> Vec<(Symbol, Decimal)> {
    let mut prices = Vec::new();
    for bar in slice.bars.values() {
        if bar.close > Decimal::ZERO {
            prices.push((bar.symbol.clone(), bar.close));
        }
    }
    for quote_bar in slice.quote_bars.values() {
        let price = quote_bar.mid_close();
        if price > Decimal::ZERO {
            prices.push((quote_bar.symbol.clone(), price));
        }
    }
    for ticks in slice.ticks.values() {
        if let Some(tick) = ticks.iter().rev().find(|tick| tick.value > Decimal::ZERO) {
            prices.push((tick.symbol.clone(), tick.value));
        }
    }
    for context in slice.perpetual_contexts.values() {
        if context.mark_px > Decimal::ZERO {
            prices.push((context.symbol.clone(), context.mark_px));
        }
    }
    for book in slice.order_books.values() {
        let price = book.mid_price();
        if price > Decimal::ZERO {
            prices.push((book.symbol.clone(), price));
        }
    }
    prices
}

fn update_live_prices(
    adapter: &PyAlgorithmAdapter,
    portfolio: &Arc<SecurityPortfolioManager>,
    slice: &Slice,
) {
    for quote_bar in slice.quote_bars.values() {
        let bid = quote_bar
            .bid
            .as_ref()
            .map(|bar| bar.close)
            .unwrap_or(Decimal::ZERO);
        let ask = quote_bar
            .ask
            .as_ref()
            .map(|bar| bar.close)
            .unwrap_or(Decimal::ZERO);
        if bid > Decimal::ZERO || ask > Decimal::ZERO {
            adapter
                .inner
                .lock()
                .unwrap()
                .securities
                .update_quote(&quote_bar.symbol, bid, ask);
        }
    }

    for (symbol, price) in live_prices_from_slice(slice) {
        adapter
            .inner
            .lock()
            .unwrap()
            .securities
            .update_price(&symbol, price);
        portfolio.update_prices(&symbol, price);
    }
    portfolio.apply_margin_interest_rates(slice.margin_interest_rates.values());
}

#[derive(Default)]
struct LiveFillForwardState {
    trade_bars: HashMap<u64, TradeBar>,
    quote_bars: HashMap<u64, QuoteBar>,
}

impl LiveFillForwardState {
    fn apply(&mut self, slice: &Slice, subscriptions: &[Arc<SubscriptionDataConfig>]) -> Slice {
        let mut filled = slice.clone();

        for subscription in subscriptions {
            if !subscription.fill_data_forward {
                continue;
            }
            match subscription.tick_type {
                TickType::Trade => {
                    let sid = subscription.symbol.id.sid;
                    if filled.bars.contains_key(&sid) {
                        continue;
                    }
                    if let Some(last) = self.trade_bars.get(&sid) {
                        filled.add_bar(fill_forward_trade_bar(last, slice.time));
                    }
                }
                TickType::Quote => {
                    let sid = subscription.symbol.id.sid;
                    if filled.quote_bars.contains_key(&sid) {
                        continue;
                    }
                    if let Some(last) = self.quote_bars.get(&sid) {
                        filled.add_quote_bar(fill_forward_quote_bar(last, slice.time));
                    }
                }
                TickType::OpenInterest => {}
            }
        }

        self.observe(&filled);
        filled
    }

    fn observe(&mut self, slice: &Slice) {
        for (&sid, bar) in &slice.bars {
            self.trade_bars.insert(sid, bar.clone());
        }
        for (&sid, bar) in &slice.quote_bars {
            self.quote_bars.insert(sid, bar.clone());
        }
    }
}

fn fill_forward_trade_bar(last: &TradeBar, frontier: DateTime) -> TradeBar {
    let mut bar = last.clone();
    bar.end_time = frontier;
    bar.time = frontier - bar.period;
    bar
}

fn fill_forward_quote_bar(last: &QuoteBar, frontier: DateTime) -> QuoteBar {
    let mut bar = last.clone();
    bar.end_time = frontier;
    bar.time = frontier - bar.period;
    bar
}

fn bars_for_live_order_processing(slice: &Slice) -> HashMap<u64, TradeBar> {
    let mut bars = slice.bars.clone();
    for quote_bar in slice.quote_bars.values() {
        if bars.contains_key(&quote_bar.symbol.id.sid) {
            continue;
        }
        let price = quote_bar.mid_close();
        if price <= Decimal::ZERO {
            continue;
        }
        bars.insert(
            quote_bar.symbol.id.sid,
            TradeBar::new(
                quote_bar.symbol.clone(),
                quote_bar.time,
                quote_bar.period,
                TradeBarData::new(price, price, price, price, Decimal::ZERO),
            ),
        );
    }
    bars
}

fn retain_brokerage_executable_paper_fills(
    algorithm: &QcAlgorithm,
    order_processor: &OrderProcessor,
    fill_events: &mut Vec<OrderEvent>,
) {
    fill_events.retain(|event| {
        if !event.is_fill() {
            return true;
        }
        order_processor
            .transaction_manager
            .get_order(event.order_id)
            .map(|order| algorithm.can_execute_order_with_brokerage_model(&order))
            .unwrap_or(true)
    });
}

fn apply_live_brokerage_model(algorithm: &mut QcAlgorithm, brokerage_model: Option<BrokerageName>) {
    if let Some(brokerage_model) = brokerage_model {
        algorithm.set_brokerage_model(brokerage_model, AccountType::Margin);
    }
}

fn submit_framework_order_requests(adapter: &PyAlgorithmAdapter, slice: &Slice) {
    let order_requests = run_framework_pipeline(&adapter.framework, &adapter.inner, slice);
    if order_requests.is_empty() {
        return;
    }

    let mut alg = adapter.inner.lock().unwrap();
    for request in order_requests {
        submit_execution_order_request(&mut alg, request);
    }
}

fn submit_execution_order_request(alg: &mut QcAlgorithm, request: lean_execution::OrderRequest) {
    if request.cancel_open_orders {
        for order in alg.transactions.get_open_orders() {
            if order.symbol.id.sid == request.symbol.id.sid {
                alg.transactions.request_cancel_order(
                    order.id,
                    alg.utc_time,
                    format!("{} replace open order", request.tag),
                );
            }
        }
    }

    use lean_execution::ExecutionOrderType;
    match request.order_type {
        ExecutionOrderType::Market => {
            alg.market_order(&request.symbol, request.quantity);
        }
        ExecutionOrderType::Limit => {
            if let Some(limit_price) = request.limit_price {
                alg.limit_order_with_properties(
                    &request.symbol,
                    request.quantity,
                    limit_price,
                    None,
                    false,
                    request.post_only,
                );
            }
        }
        ExecutionOrderType::MarketOnOpen => {
            alg.market_on_open_order(&request.symbol, request.quantity);
        }
        ExecutionOrderType::MarketOnClose => {
            alg.market_on_close_order(&request.symbol, request.quantity);
        }
        ExecutionOrderType::Cancel => {}
    }
}

fn process_live_universe_data(
    adapter: &mut PyAlgorithmAdapter,
    slice_proxy: &mut SliceProxy,
    portfolio: &Arc<SecurityPortfolioManager>,
    subscriptions: &mut Vec<Arc<SubscriptionDataConfig>>,
    loaded_subscription_ids: &mut HashSet<u64>,
    active: &mut Vec<ActiveLiveSubscription>,
    live_data_queue: &mut DataQueueHandlerManager,
    source_type: String,
    ticker: String,
    resolution: Resolution,
    time: DateTime,
    data: Vec<CustomDataPoint>,
) -> Result<()> {
    set_algorithm_time(adapter, time);
    debug!(
        "Live universe data received: {}:{} resolution={:?} rows={} time={}",
        source_type,
        ticker,
        resolution,
        data.len(),
        time
    );

    let funding_delta = apply_live_universe_margin_interest_rates(portfolio, time, &data);
    let mut universe_data = HashMap::new();
    universe_data.insert(ticker.to_ascii_uppercase(), data);
    Python::attach(|py| {
        adapter.apply_custom_universe_selection(py, time.0, resolution, &universe_data)
    });
    if funding_delta != Decimal::ZERO {
        info!(
            "Live funding cash delta from {}:{} at {}: {}",
            source_type, ticker, time, funding_delta
        );
    }

    if reconcile_live_market_subscriptions(
        adapter,
        subscriptions,
        loaded_subscription_ids,
        active,
        live_data_queue,
    )? {
        *slice_proxy = Python::attach(|py| SliceProxy::new(py, subscriptions))?;
    }

    Ok(())
}

fn apply_live_universe_margin_interest_rates(
    portfolio: &Arc<SecurityPortfolioManager>,
    time: DateTime,
    data: &[CustomDataPoint],
) -> Decimal {
    data.iter()
        .filter_map(|point| margin_interest_rate_from_universe_point(time, point))
        .filter_map(|rate| portfolio.apply_margin_interest_rate(&rate))
        .sum()
}

fn margin_interest_rate_from_universe_point(
    time: DateTime,
    point: &CustomDataPoint,
) -> Option<MarginInterestRate> {
    let symbol_value = json_string_field(&point.fields, "symbol")?;
    let funding = json_decimal_field(&point.fields, "funding")?;
    let market = json_string_field(&point.fields, "market")
        .map(Market::new)
        .unwrap_or_else(|| Market::new(Market::HYPERLIQUID));
    let security_type = json_string_field(&point.fields, "security_type")
        .unwrap_or_else(|| "CryptoFuture".to_string())
        .to_ascii_lowercase();
    let symbol = match security_type.as_str() {
        "crypto" => Symbol::create_crypto(&symbol_value, &market),
        "cryptofuture" | "crypto_future" | "crypto-future" => {
            Symbol::create_crypto_future(&symbol_value, &market)
        }
        _ => return None,
    };
    Some(MarginInterestRate::new(symbol, time, funding))
}

fn json_string_field(fields: &HashMap<String, serde_json::Value>, name: &str) -> Option<String> {
    fields.get(name)?.as_str().map(str::to_string)
}

fn json_decimal_field(fields: &HashMap<String, serde_json::Value>, name: &str) -> Option<Decimal> {
    let value = fields.get(name)?;
    if let Some(number) = value.as_f64() {
        return Decimal::from_f64(number);
    }
    value.as_str()?.trim().parse::<Decimal>().ok()
}

#[allow(clippy::too_many_arguments)]
fn process_live_slice(
    adapter: &mut PyAlgorithmAdapter,
    slice_proxy: &mut SliceProxy,
    slice: &Slice,
    subscriptions: &mut Vec<Arc<SubscriptionDataConfig>>,
    loaded_subscription_ids: &mut HashSet<u64>,
    active: &mut Vec<ActiveLiveSubscription>,
    live_data_queue: &mut DataQueueHandlerManager,
    live_brokerage: Option<&mut LiveBrokerageBridge>,
    order_processor: &OrderProcessor,
    portfolio: &Arc<SecurityPortfolioManager>,
    live_writer: Option<&LiveDeploymentWriter>,
    all_order_events: &mut Vec<OrderEvent>,
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
) -> Result<()> {
    set_algorithm_time(adapter, slice.time);
    let live_slice = slice.clone();

    update_live_prices(adapter, portfolio, &live_slice);

    Python::attach(|py| {
        slice_proxy.update(py, &live_slice);
        slice_proxy.update_quote_bars(py, &live_slice.quote_bars);
        slice_proxy.update_margin_interest_rates(py, &live_slice);
        slice_proxy.update_perpetual_contexts(py, &live_slice);
        slice_proxy.update_ticks(py, &live_slice.ticks);
        slice_proxy.update_custom_data(py, &live_slice.custom_data);
        adapter.on_data_proxy(py, slice_proxy, &live_slice);
    });

    submit_framework_order_requests(adapter, &live_slice);

    let use_paper_fills = live_brokerage
        .as_ref()
        .map(|bridge| bridge.paper_fills)
        .unwrap_or(true);

    if reconcile_live_market_subscriptions(
        adapter,
        subscriptions,
        loaded_subscription_ids,
        active,
        live_data_queue,
    )? {
        *slice_proxy = Python::attach(|py| SliceProxy::new(py, subscriptions))?;
    }

    if let Some(bridge) = live_brokerage {
        let mut brokerage_events = bridge.submit_new_orders(order_processor, live_slice.time)?;
        if !brokerage_events.is_empty() {
            for event in &brokerage_events {
                info!(
                    "Live brokerage event: order_id={} symbol={} status={:?} message={}",
                    event.order_id, event.symbol.value, event.status, event.message
                );
            }
            OrderEventProcessingContext {
                adapter,
                portfolio,
                order_processor,
                all_order_events,
                trade_builder,
                completed_trades,
                live_writer: as_sidecar_writer(live_writer),
            }
            .process(&mut brokerage_events);
        }

        let mut update_events = bridge.process_update_requests(order_processor, live_slice.time)?;
        if !update_events.is_empty() {
            for event in &update_events {
                info!(
                    "Live update event: order_id={} symbol={} status={:?} message={}",
                    event.order_id, event.symbol.value, event.status, event.message
                );
            }
            OrderEventProcessingContext {
                adapter,
                portfolio,
                order_processor,
                all_order_events,
                trade_builder,
                completed_trades,
                live_writer: as_sidecar_writer(live_writer),
            }
            .process(&mut update_events);
        }

        let mut cancel_events = bridge.process_cancel_requests(order_processor, live_slice.time)?;
        if !cancel_events.is_empty() {
            for event in &cancel_events {
                info!(
                    "Live cancel event: order_id={} symbol={} status={:?} message={}",
                    event.order_id, event.symbol.value, event.status, event.message
                );
            }
            OrderEventProcessingContext {
                adapter,
                portfolio,
                order_processor,
                all_order_events,
                trade_builder,
                completed_trades,
                live_writer: as_sidecar_writer(live_writer),
            }
            .process(&mut cancel_events);
        }

        if !bridge.paper_fills {
            let mut broker_snapshot_events =
                bridge.reconcile_order_events(order_processor, live_slice.time);
            if !broker_snapshot_events.is_empty() {
                for event in &broker_snapshot_events {
                    info!(
                        "Live brokerage snapshot event: order_id={} symbol={} status={:?} fill_qty={} fill_price={} message={}",
                        event.order_id,
                        event.symbol.value,
                        event.status,
                        event.fill_quantity,
                        event.fill_price,
                        event.message
                    );
                }
                OrderEventProcessingContext {
                    adapter,
                    portfolio,
                    order_processor,
                    all_order_events,
                    trade_builder,
                    completed_trades,
                    live_writer: as_sidecar_writer(live_writer),
                }
                .process(&mut broker_snapshot_events);
            }
        }
    } else {
        let mut submit_events =
            drain_local_new_orders(order_processor, live_slice.time, "local paper brokerage");
        if !submit_events.is_empty() {
            for event in &submit_events {
                info!(
                    "Live brokerage event: order_id={} symbol={} status={:?} message={}",
                    event.order_id, event.symbol.value, event.status, event.message
                );
            }
            OrderEventProcessingContext {
                adapter,
                portfolio,
                order_processor,
                all_order_events,
                trade_builder,
                completed_trades,
                live_writer: as_sidecar_writer(live_writer),
            }
            .process(&mut submit_events);
        }

        let mut update_events =
            drain_local_update_requests(order_processor, live_slice.time, "local paper fills");
        if !update_events.is_empty() {
            OrderEventProcessingContext {
                adapter,
                portfolio,
                order_processor,
                all_order_events,
                trade_builder,
                completed_trades,
                live_writer: as_sidecar_writer(live_writer),
            }
            .process(&mut update_events);
        }

        let mut cancel_events =
            drain_local_cancel_requests(order_processor, live_slice.time, "local paper fills");
        if !cancel_events.is_empty() {
            OrderEventProcessingContext {
                adapter,
                portfolio,
                order_processor,
                all_order_events,
                trade_builder,
                completed_trades,
                live_writer: as_sidecar_writer(live_writer),
            }
            .process(&mut cancel_events);
        }
    }

    if use_paper_fills {
        let bars_for_orders = bars_for_live_order_processing(&live_slice);
        let mut fill_events = order_processor.generate_order_events_with_quotes(
            &bars_for_orders,
            &live_slice.quote_bars,
            live_slice.time,
        );
        {
            let algorithm = adapter.inner.lock().unwrap();
            retain_brokerage_executable_paper_fills(&algorithm, order_processor, &mut fill_events);
        }
        if !fill_events.is_empty() {
            for event in &fill_events {
                info!(
                    "Live order event: order_id={} symbol={} status={:?} fill_qty={} fill_price={}",
                    event.order_id,
                    event.symbol.value,
                    event.status,
                    event.fill_quantity,
                    event.fill_price
                );
            }
        }
        OrderEventProcessingContext {
            adapter,
            portfolio,
            order_processor,
            all_order_events,
            trade_builder,
            completed_trades,
            live_writer: as_sidecar_writer(live_writer),
        }
        .process(&mut fill_events);
    }

    Ok(())
}

/// Run a Python strategy against live provider streams.
///
/// The runner follows the C# LEAN live path: data queue subscriptions feed a
/// frontier assembler, each completed frontier is delivered through `OnData`,
/// then paper-mode order fills are scanned against that same frontier.
pub async fn run_live_strategy(
    strategy_path: &Path,
    mut config: LiveRunConfig,
) -> Result<LiveRunResult> {
    let started_at = chrono::Utc::now();
    let mut adapter = Python::attach(|py| load_strategy(py, strategy_path))?;

    Python::attach(|py| {
        adapter.set_history_context(
            py,
            AlgorithmHistoryContext {
                data_root: config.data_root.clone(),
                history_provider: config.history_provider.clone(),
                custom_data_sources: config.custom_data_sources.clone(),
            },
        )
    })?;
    Python::attach(|py| adapter.set_parameters(py, config.parameters.clone()))?;

    {
        let mut algorithm = adapter.inner.lock().unwrap();
        apply_live_brokerage_model(&mut algorithm, config.brokerage_model);
    }

    adapter
        .initialize()
        .context("strategy initialize() failed")?;

    let live_job_brokerage = config
        .brokerage
        .as_ref()
        .map(|brokerage| brokerage.name().to_string())
        .unwrap_or_else(|| {
            if config.paper_trading {
                "Paper".to_string()
            } else {
                "Plugin".to_string()
            }
        });
    let job = lean_data::LiveNodePacket {
        brokerage: live_job_brokerage,
        parameters: config.parameters.clone(),
        paper_trading: config.paper_trading,
        ..Default::default()
    };
    config.live_data_queue.set_job(&job)?;

    let portfolio = adapter.inner.lock().unwrap().portfolio.clone();
    let transactions = adapter.inner.lock().unwrap().transactions.clone();
    let order_processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        transactions,
    );
    let live_writer = config.output_dir.clone().map(LiveDeploymentWriter::new);
    let mut live_brokerage = config
        .brokerage
        .take()
        .map(|brokerage| LiveBrokerageBridge::connect(brokerage, config.paper_trading))
        .transpose()?;
    if let Some(bridge) = live_brokerage.as_ref() {
        let account_state = bridge.sync_account_state()?;
        info!(
            "Synchronized live brokerage account state: cash={} holdings={} open_orders={}",
            account_state.cash,
            account_state.holdings.len(),
            account_state.open_orders.len()
        );
        apply_initial_brokerage_account_state(&adapter.inner, &account_state);
    }

    let mut subscriptions: Vec<Arc<SubscriptionDataConfig>> =
        adapter.inner.lock().unwrap().subscription_manager.get_all();
    let mut loaded_subscription_ids = HashSet::new();
    let mut active = Vec::new();

    for subscription in &subscriptions {
        loaded_subscription_ids.insert(subscription.unique_id());
        let live_subscription = config.live_data_queue.subscribe(subscription.as_ref())?;
        add_active_live_subscription(&mut active, live_subscription);
    }

    let custom_subscriptions = adapter
        .inner
        .lock()
        .unwrap()
        .custom_data_subscriptions
        .clone();
    let live_universe_subscriptions = collect_live_universe_subscriptions(&adapter);
    info!(
        "Live subscriptions after initialize: market={} custom={} universe={}",
        subscriptions.len(),
        custom_subscriptions
            .iter()
            .filter(|subscription| !subscription.is_universe())
            .count(),
        live_universe_subscriptions.len()
    );
    let custom_data_sources = config.custom_data_sources.clone();
    let live_data_root = config.data_root.clone();
    for custom_subscription in custom_subscriptions
        .iter()
        .filter(|subscription| !subscription.is_universe())
    {
        let live_subscription = subscribe_live_custom_data(
            &mut config.live_data_queue,
            &custom_data_sources,
            live_data_root.clone(),
            custom_subscription,
        )?;
        add_active_live_subscription(&mut active, live_subscription);
    }
    for universe_subscription in &live_universe_subscriptions {
        let live_subscription = subscribe_live_universe_data(
            &mut config.live_data_queue,
            &custom_data_sources,
            live_data_root.clone(),
            universe_subscription,
        )?;
        add_active_live_subscription(&mut active, live_subscription);
    }

    if active.is_empty() {
        anyhow::bail!(
            "live strategy has no market, custom, or universe subscriptions after initialize()"
        );
    }

    let mut slice_proxy = Python::attach(|py| SliceProxy::new(py, &subscriptions))
        .context("Failed to create live SliceProxy")?;
    let mut assembler = LiveSliceAssembler::new();
    let mut live_fill_forward = LiveFillForwardState::default();
    let mut slices_processed = 0usize;
    let mut all_order_events = Vec::new();
    let mut trade_builder = TradeBuilder::default();
    let mut completed_trades = Vec::new();
    let runtime_started = Instant::now();
    if let Some(writer) = &live_writer {
        writer.record_snapshot(
            DateTime::now(),
            &portfolio,
            &order_processor,
            slices_processed,
            all_order_events.len(),
            completed_trades.len(),
        );
    }

    loop {
        if config
            .max_runtime
            .map(|max_runtime| runtime_started.elapsed() >= max_runtime)
            .unwrap_or(false)
        {
            if let Some(slice) = assembler.flush() {
                let slice = live_fill_forward.apply(&slice, &subscriptions);
                process_live_slice(
                    &mut adapter,
                    &mut slice_proxy,
                    &slice,
                    &mut subscriptions,
                    &mut loaded_subscription_ids,
                    &mut active,
                    &mut config.live_data_queue,
                    live_brokerage.as_mut(),
                    &order_processor,
                    &portfolio,
                    live_writer.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                )?;
                slices_processed += 1;
                if let Some(writer) = &live_writer {
                    writer.record_snapshot(
                        slice.time,
                        &portfolio,
                        &order_processor,
                        slices_processed,
                        all_order_events.len(),
                        completed_trades.len(),
                    );
                }
            }
            let stopped_at = chrono::Utc::now();
            if let Some(writer) = &live_writer {
                writer.mark_stopped(
                    "stopped",
                    &portfolio,
                    slices_processed,
                    all_order_events.len(),
                    completed_trades.len(),
                );
            }
            return Ok(LiveRunResult {
                slices_processed,
                final_value: portfolio.total_portfolio_value().to_f64().unwrap_or(0.0),
                order_events: all_order_events,
                started_at,
                stopped_at,
            });
        }

        if active.is_empty() {
            if let Some(slice) = assembler.flush() {
                let slice = live_fill_forward.apply(&slice, &subscriptions);
                process_live_slice(
                    &mut adapter,
                    &mut slice_proxy,
                    &slice,
                    &mut subscriptions,
                    &mut loaded_subscription_ids,
                    &mut active,
                    &mut config.live_data_queue,
                    live_brokerage.as_mut(),
                    &order_processor,
                    &portfolio,
                    live_writer.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                )?;
                slices_processed += 1;
                if let Some(writer) = &live_writer {
                    writer.record_snapshot(
                        slice.time,
                        &portfolio,
                        &order_processor,
                        slices_processed,
                        all_order_events.len(),
                        completed_trades.len(),
                    );
                }
            }
            break;
        }

        let mut select = crossbeam_channel::Select::new();
        for subscription in &active {
            select.recv(&subscription.receiver);
        }
        let selected = match select.select_timeout(LIVE_SYNCHRONIZER_HEARTBEAT) {
            Ok(operation) => {
                let index = operation.index();
                let received = operation.recv(&active[index].receiver);
                Some((index, received))
            }
            Err(_) => None,
        };

        let Some((index, received)) = selected else {
            if let Some(slice) = assembler.flush_ready(DateTime::now()) {
                let slice = live_fill_forward.apply(&slice, &subscriptions);
                process_live_slice(
                    &mut adapter,
                    &mut slice_proxy,
                    &slice,
                    &mut subscriptions,
                    &mut loaded_subscription_ids,
                    &mut active,
                    &mut config.live_data_queue,
                    live_brokerage.as_mut(),
                    &order_processor,
                    &portfolio,
                    live_writer.as_ref(),
                    &mut all_order_events,
                    &mut trade_builder,
                    &mut completed_trades,
                )?;
                slices_processed += 1;
                if let Some(writer) = &live_writer {
                    writer.record_snapshot(
                        slice.time,
                        &portfolio,
                        &order_processor,
                        slices_processed,
                        all_order_events.len(),
                        completed_trades.len(),
                    );
                }
                if config
                    .max_slices
                    .map(|max_slices| slices_processed >= max_slices)
                    .unwrap_or(false)
                {
                    let stopped_at = chrono::Utc::now();
                    if let Some(writer) = &live_writer {
                        writer.mark_stopped(
                            "stopped",
                            &portfolio,
                            slices_processed,
                            all_order_events.len(),
                            completed_trades.len(),
                        );
                    }
                    return Ok(LiveRunResult {
                        slices_processed,
                        final_value: portfolio.total_portfolio_value().to_f64().unwrap_or(0.0),
                        order_events: all_order_events,
                        started_at,
                        stopped_at,
                    });
                }
            } else if let Some(writer) = &live_writer {
                writer.append_heartbeat(
                    DateTime::now(),
                    &portfolio,
                    slices_processed,
                    all_order_events.len(),
                    completed_trades.len(),
                );
            }
            continue;
        };

        match received {
            Ok(Ok(LiveDataItem::UniverseData {
                source_type,
                ticker,
                resolution,
                time,
                data,
            })) => {
                process_live_universe_data(
                    &mut adapter,
                    &mut slice_proxy,
                    &portfolio,
                    &mut subscriptions,
                    &mut loaded_subscription_ids,
                    &mut active,
                    &mut config.live_data_queue,
                    source_type,
                    ticker,
                    resolution,
                    time,
                    data,
                )?;
                if let Some(writer) = &live_writer {
                    writer.record_snapshot(
                        time,
                        &portfolio,
                        &order_processor,
                        slices_processed,
                        all_order_events.len(),
                        completed_trades.len(),
                    );
                }
            }
            Ok(Ok(item)) => {
                for slice in assembler.push(item) {
                    let slice = live_fill_forward.apply(&slice, &subscriptions);
                    process_live_slice(
                        &mut adapter,
                        &mut slice_proxy,
                        &slice,
                        &mut subscriptions,
                        &mut loaded_subscription_ids,
                        &mut active,
                        &mut config.live_data_queue,
                        live_brokerage.as_mut(),
                        &order_processor,
                        &portfolio,
                        live_writer.as_ref(),
                        &mut all_order_events,
                        &mut trade_builder,
                        &mut completed_trades,
                    )?;
                    slices_processed += 1;
                    if let Some(writer) = &live_writer {
                        writer.record_snapshot(
                            slice.time,
                            &portfolio,
                            &order_processor,
                            slices_processed,
                            all_order_events.len(),
                            completed_trades.len(),
                        );
                    }
                    if config
                        .max_slices
                        .map(|max_slices| slices_processed >= max_slices)
                        .unwrap_or(false)
                    {
                        let stopped_at = chrono::Utc::now();
                        if let Some(writer) = &live_writer {
                            writer.mark_stopped(
                                "stopped",
                                &portfolio,
                                slices_processed,
                                all_order_events.len(),
                                completed_trades.len(),
                            );
                        }
                        return Ok(LiveRunResult {
                            slices_processed,
                            final_value: portfolio.total_portfolio_value().to_f64().unwrap_or(0.0),
                            order_events: all_order_events,
                            started_at,
                            stopped_at,
                        });
                    }
                }
            }
            Ok(Err(err)) => return Err(err.into()),
            Err(_) => {
                active.remove(index);
            }
        }
    }

    let stopped_at = chrono::Utc::now();
    if let Some(writer) = &live_writer {
        writer.mark_stopped(
            "stopped",
            &portfolio,
            slices_processed,
            all_order_events.len(),
            completed_trades.len(),
        );
    }
    Ok(LiveRunResult {
        slices_processed,
        final_value: portfolio.total_portfolio_value().to_f64().unwrap_or(0.0),
        order_events: all_order_events,
        started_at,
        stopped_at,
    })
}

/// Run the full backtest loop for a Python strategy.
///
/// Must be called from within an existing tokio runtime (e.g. via `.await`).
/// Do NOT decorate call-sites with `#[tokio::main]` — the caller's runtime
/// is reused so that tokio primitives (Mutex, Semaphore, reqwest) in the
/// historical provider work correctly across the same runtime context.
pub async fn run_strategy(strategy_path: &Path, config: RunConfig) -> Result<BacktestResult> {
    let mut adapter = Python::attach(|py| load_strategy(py, strategy_path))?;

    Python::attach(|py| {
        adapter.set_history_context(
            py,
            AlgorithmHistoryContext {
                data_root: config.data_root.clone(),
                history_provider: config.history_provider.clone(),
                custom_data_sources: config.custom_data_sources.clone(),
            },
        )
    })?;
    Python::attach(|py| adapter.set_parameters(py, config.parameters.clone()))?;

    // ── initialize ──────────────────────────────────────────────────────────
    adapter
        .initialize()
        .context("strategy initialize() failed")?;

    let start_date = config
        .start_date_override
        .unwrap_or_else(|| adapter.start_date().date_utc());
    let strategy_end_date = adapter.end_date();
    let end_date = resolve_backtest_end_date(
        config.end_date_override,
        strategy_end_date,
        Local::now().date_naive(),
    );
    if config.end_date_override.is_none() && strategy_end_date == DateTime::MAX {
        info!("No strategy end date specified; defaulting backtest end date to {end_date}");
    }

    let starting_cash = {
        use rust_decimal::prelude::ToPrimitive;
        adapter
            .inner
            .lock()
            .unwrap()
            .portfolio_value()
            .to_f64()
            .unwrap_or(100_000.0)
    };

    // ── gather subscriptions ────────────────────────────────────────────────
    let mut subscriptions: Vec<Arc<SubscriptionDataConfig>> =
        { adapter.inner.lock().unwrap().subscription_manager.get_all() };

    let initial_custom_subs: Vec<CustomDataSubscription> = {
        adapter
            .inner
            .lock()
            .unwrap()
            .custom_data_subscriptions
            .clone()
    };
    if !initial_custom_subs.is_empty() {
        let universe_data_for_start =
            load_low_resolution_universe_data_for_day(&initial_custom_subs, &config, start_date)
                .await?;
        if !universe_data_for_start.is_empty() {
            Python::attach(|py| {
                adapter.apply_custom_universe_selection(
                    py,
                    date_to_datetime(start_date, 0, 0, 0).0,
                    Resolution::Daily,
                    &universe_data_for_start,
                )
            });
            subscriptions = adapter.inner.lock().unwrap().subscription_manager.get_all();
        }
    }

    let has_universes = { !adapter.universes.lock().unwrap().is_empty() };
    if subscriptions.is_empty() && !has_universes {
        warn!("No subscriptions — strategy did not call add_equity/add_forex.");
    }

    // ── determine effective benchmark ticker ────────────────────────────────
    // Use the symbol set by set_benchmark(), or fall back to SPY.
    let effective_benchmark_ticker: String = {
        adapter
            .inner
            .lock()
            .unwrap()
            .benchmark_symbol
            .clone()
            .unwrap_or_else(|| "SPY".to_string())
    };

    // Resolve the benchmark like LEAN does: prefer an existing security, and
    // only create a fallback symbol when the algorithm has not already
    // subscribed to it. Provider-specific symbols such as XYZ:SP500 must keep
    // their real security type/market so the data resolver does not look under
    // equity/usa.
    let benchmark_symbol_obj =
        resolve_benchmark_symbol(&effective_benchmark_ticker, &subscriptions);
    let benchmark_in_subs =
        benchmark_symbol_in_subscriptions(&benchmark_symbol_obj, &subscriptions);

    info!(
        "Benchmark: {} ({})",
        effective_benchmark_ticker,
        if benchmark_in_subs {
            "already subscribed"
        } else {
            "internal subscription"
        }
    );

    // ── build infrastructure ────────────────────────────────────────────────
    let reader = Arc::new(ParquetReader::new());
    let resolver = PathResolver::new(config.data_root.clone());
    let cache = DataCache::new(50_000);
    let transactions = adapter.inner.lock().unwrap().transactions.clone();
    let portfolio = adapter.inner.lock().unwrap().portfolio.clone();

    let order_processor = OrderProcessor::new(
        Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
        transactions,
    );

    // ── determine warm-up window ────────────────────────────────────────────
    // Compute this before prefetching so data is requested once for the full
    // range the algorithm can consume. This mirrors LEAN's source-driven data
    // provider path and keeps the date-partitioned cache complete for warm-up
    // plus the main backtest period.
    let warmup_start: Option<NaiveDate> = {
        let alg = adapter.inner.lock().unwrap();
        if let Some(bar_count) = alg.warmup_bar_count {
            // C# LEAN counts back N trading days using exchange calendar.
            // For daily data: 5 trading days per 7 calendar days -> multiply by 7/5.
            // Add a small buffer (+10) to ensure we never undershoot.
            let calendar_days = (bar_count as i64 * 7 + 4) / 5 + 10;
            Some(start_date - chrono::Duration::days(calendar_days))
        } else if let Some(dur) = alg.warmup_duration {
            let days = (dur.nanos / TimeSpan::ONE_DAY.nanos).max(1);
            Some(start_date - chrono::Duration::days(days))
        } else if let Some(period) = alg.warmup_period {
            let days = (period.nanos / TimeSpan::ONE_DAY.nanos).max(1);
            Some(start_date - chrono::Duration::days(days))
        } else {
            None
        }
    };

    // ── pre-fetch missing low-resolution data ───────────────────────────────
    // C# LEAN resolves date-partitioned high-resolution sources lazily: the
    // subscription reader asks for the current tradable date's source and the
    // data provider reports whether that file exists. Mirror that behavior by
    // leaving hour/minute/second/tick data to the intraday loop below.
    if let Some(ref provider) = config.historical_provider {
        let startup_prefetch_subscriptions: Vec<_> = subscriptions
            .iter()
            .filter(|sub| !sub.resolution.is_intraday() && sub.tick_type != TickType::Quote)
            .cloned()
            .collect();

        pre_fetch_all(
            provider.clone(),
            config.history_provider.clone(),
            &startup_prefetch_subscriptions,
            warmup_start.unwrap_or(start_date),
            end_date,
            &resolver,
        )
        .await?;
    }

    if let Some(ref provider) = config.history_provider {
        ensure_auxiliary_files_for_subscriptions(
            provider.clone(),
            &subscriptions,
            start_date,
            end_date,
            &resolver,
        )
        .await?;
    }

    // ── map files: load ticker rename history ───────────────────────────────
    // Map files are Parquet; key = symbol SID → rows sorted newest first.
    // Used for mapped data range checks and to fire SymbolChangedEvent
    // (rename) and Delisting events each day.
    let factor_reader = ParquetReader::new();
    let mut map_file_map: HashMap<u64, Vec<MapFileEntry>> = HashMap::new();
    let mut loaded_map_sids: HashSet<u64> = HashSet::new();
    for sub in &subscriptions {
        ensure_map_rows_for_subscription(
            &factor_reader,
            &config.data_root,
            sub,
            &mut map_file_map,
            &mut loaded_map_sids,
        );
    }

    // ── factor files: load from disk ─────────────────────────────────────────
    // Factor files are Parquet; key = symbol SID → rows sorted newest first.
    // Generated during pre_fetch_all via DataType::FactorFile requests —
    // providers that support corporate actions (e.g. massive) handle the
    // request; those that don't (e.g. thetadata) return NotImplemented.
    let mut factor_map: HashMap<u64, Vec<FactorFileEntry>> = HashMap::new();
    let mut loaded_factor_sids: HashSet<u64> = HashSet::new();
    let require_factor_files = config.history_provider.is_some();
    for sub in &subscriptions {
        if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
            continue;
        }
        if !loaded_factor_sids.insert(sub.symbol.id.sid) {
            continue;
        }
        load_factor_rows_into_map(
            &factor_reader,
            &config.data_root,
            sub,
            map_file_map.get(&sub.symbol.id.sid).map(Vec::as_slice),
            start_date,
            end_date,
            &mut factor_map,
            require_factor_files,
        )?;
    }

    // ── option underlying SIDs: skip factor adjustment for these ─────────────
    // When a strategy subscribes to options, LEAN uses raw (unadjusted) prices
    // for the underlying equity so that strike selection matches live market
    // prices.  Build a set of equity SIDs that serve as option underlyings so
    // the bar-loading loop can bypass apply_factor_row for them.
    let option_underlying_sids: std::collections::HashSet<u64> = {
        let alg = adapter.inner.lock().unwrap();
        let mut sids = std::collections::HashSet::new();
        for canonical in &alg.option_subscriptions {
            let underlying_ticker = canonical.permtick.trim_start_matches('?');
            for sub in &subscriptions {
                if sub.symbol.permtick.eq_ignore_ascii_case(underlying_ticker) {
                    sids.insert(sub.symbol.id.sid);
                }
            }
        }
        sids
    };

    // ── exchange-hours filter map ─────────────────────────────────────────────
    // For each subscription with extended_market_hours=false, record the
    // ExchangeHours so the minute loop can drop pre-market / after-hours bars.
    // Keyed by SID; subscriptions with extended_market_hours=true are absent.
    let market_hours_filter: HashMap<u64, lean_core::exchange_hours::ExchangeHours> = {
        let alg = adapter.inner.lock().unwrap();
        let mut map = HashMap::new();
        for sub in &subscriptions {
            if !sub.extended_market_hours {
                if let Some(sec) = alg.securities.get(&sub.symbol) {
                    map.insert(sub.symbol.id.sid, sec.exchange_hours.clone());
                }
            }
        }
        map
    };

    // ── warm-up loop ────────────────────────────────────────────────────────
    if let Some(wu_start) = warmup_start {
        info!("Warm-up: {} → {} (exclusive)", wu_start, start_date);

        let mut wu_date = wu_start;
        while wu_date < start_date {
            let utc_time = date_to_datetime(wu_date, 16, 0, 0);
            set_algorithm_time(&adapter, utc_time);

            let mut slice = Slice::new(utc_time);
            for sub in &subscriptions {
                let sid = sub.symbol.id.sid;
                let day_key = day_key(wu_date);
                let path = resolver.market_data_partition(
                    &sub.symbol,
                    sub.resolution,
                    TickType::Trade,
                    wu_date,
                );

                if path.exists() {
                    let bars = if let Some(cached) = cache.get_bars(sid, day_key) {
                        cached.as_ref().clone()
                    } else {
                        let day_start = date_to_datetime(wu_date, 0, 0, 0);
                        let day_end = date_to_datetime(wu_date, 23, 59, 59);
                        let params = QueryParams::new().with_time_range(day_start, day_end);
                        let loaded = reader
                            .read_trade_bar_partition(&path, &sub.symbol, &params)
                            .unwrap_or_default();
                        cache.insert_bars(sid, day_key, loaded.clone());
                        loaded
                    };

                    for bar in bars {
                        let bar = if let Some(rows) = factor_map.get(&sid) {
                            apply_factor_row(bar, rows, wu_date)
                        } else {
                            bar
                        };
                        adapter
                            .inner
                            .lock()
                            .unwrap()
                            .securities
                            .update_price(&bar.symbol, bar.close);
                        portfolio.update_prices(&bar.symbol, bar.close);
                        slice.add_bar(bar);
                    }
                }
            }

            let custom_subs: Vec<CustomDataSubscription> = {
                adapter
                    .inner
                    .lock()
                    .unwrap()
                    .custom_data_subscriptions
                    .clone()
            };
            let custom_data_for_day = load_low_resolution_custom_data_for_day(
                &custom_subs,
                &HashMap::new(),
                &config,
                wu_date,
            )
            .await?;
            if !custom_data_for_day.is_empty() {
                slice.has_data = true;
            }

            if slice.has_data {
                // During warm-up: call on_data for indicator updates only.
                // Orders are NOT processed; equity is NOT recorded.
                adapter.on_data_with_custom(&slice, &custom_data_for_day);
            }

            wu_date += chrono::Duration::days(1);
        }

        // Signal end of warm-up.
        adapter.inner.lock().unwrap().end_warm_up();
        adapter.on_warmup_finished();
        info!("Warm-up complete.");
    } else {
        // C# LEAN calls OnWarmupFinished even when no warm-up period is set.
        // Python strategies commonly implement the snake_case hook; the
        // QCAlgorithm attribute bridge resolves this PascalCase dispatch.
        adapter.on_warmup_finished();
    }

    // ── detect resolution mode ───────────────────────────────────────────────
    let highest_universe_resolution = Python::attach(|py| {
        adapter
            .universes
            .lock()
            .unwrap()
            .iter()
            .map(|universe| universe.bind(py).borrow().settings().resolution)
            .min()
    });
    let is_intraday = subscriptions.iter().any(|s| s.resolution.is_intraday())
        || highest_universe_resolution.is_some_and(|resolution| resolution.is_intraday());

    // ── pre-load all subscription bars (daily mode only) ─────────────────────
    // For daily resolutions, pre-loading the date-partitioned cache up front
    // keeps the main loop to in-memory lookups per subscription/date.
    //
    // bar_map and subscriptions are mut because strategies may call add_equity()
    // mid-backtest (dynamic universe selection).  New subscriptions are detected
    // at the start of each trading day and their bars are lazy-loaded here.
    let daily_full_params = QueryParams::new().with_time_range(
        date_to_datetime(start_date, 0, 0, 0),
        date_to_datetime(end_date, 23, 59, 59),
    );
    let mut bar_map: HashMap<u64, HashMap<chrono::NaiveDate, lean_data::TradeBar>> = if !is_intraday
    {
        let mut map = HashMap::new();
        for sub in &subscriptions {
            let sid = sub.symbol.id.sid;
            let bars = load_trade_bar_partitions(
                &reader,
                &resolver,
                sub,
                start_date,
                end_date,
                &daily_full_params,
            );
            if !bars.is_empty() {
                let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> =
                    bars.into_iter().map(|b| (b.time.date_utc(), b)).collect();
                info!(
                    "Pre-loaded {} bars for {}",
                    date_map.len(),
                    sub.symbol.value
                );
                map.insert(sid, date_map);
            }
        }
        map
    } else {
        HashMap::new()
    };
    // Track full subscription configs, not just SIDs: LEAN can maintain both
    // Trade and Quote subscriptions for the same symbol at intraday resolution.
    let mut loaded_subscription_ids: std::collections::HashSet<u64> =
        subscriptions.iter().map(|s| s.unique_id()).collect();

    // ── pre-allocate proxy objects for the hot path ──────────────────────────
    // One PyTradeBar per subscription is allocated here and reused every day.
    // `on_data_proxy` updates fields in-place instead of constructing new objects.
    let mut slice_proxy = Python::attach(|py| SliceProxy::new(py, &subscriptions))
        .context("Failed to create SliceProxy")?;

    // ── pre-load benchmark data ──────────────────────────────────────────────
    let benchmark_sid: u64 = benchmark_symbol_obj.id.sid;
    let mut benchmark_curve: Vec<Decimal> = Vec::new();
    let mut benchmark_dates: Vec<String> = Vec::new();
    let benchmark_price_map: HashMap<NaiveDate, Decimal> = if benchmark_in_subs {
        HashMap::new()
    } else {
        load_internal_benchmark_price_map(
            config.historical_provider.clone(),
            reader.as_ref(),
            &resolver,
            &benchmark_symbol_obj,
            start_date,
            end_date,
        )
        .await
    };

    // TradeBuilder assembles completed round-trip trades from fills.
    let mut trade_builder = TradeBuilder::new();
    let mut completed_trades: Vec<Trade> = Vec::new();

    // Collect all order events emitted during the backtest.
    let mut all_order_events: Vec<OrderEvent> = Vec::new();

    // Data request tracking: record which symbol+date combinations had data and which did not.
    let mut succeeded_data_requests: Vec<String> = Vec::new();
    let mut failed_data_requests: Vec<String> = Vec::new();

    // Record the backtest start time as Unix epoch seconds (used as the LEAN backtest ID).
    let backtest_id = chrono::Utc::now().timestamp();
    let mut live_writer = config
        .output_dir
        .clone()
        .map(|dir| LiveBacktestWriter::new(dir, start_date, end_date));

    // ── prefetch full-history custom data sources ────────────────────────────
    // For sources where is_full_history_source() is true (FRED, CBOE VIX, …),
    // download the full series once, cache to history.parquet, and load the
    // entire series into memory so the loop can look up by date without any
    // per-day I/O or HTTP calls.
    //
    // Uses async reqwest (not blocking) so that HTTP/2 (required by some
    // providers like FRED) works correctly inside the tokio runtime.
    //
    // key: ticker (uppercased) → date → points for that date
    let custom_history: HashMap<String, HashMap<NaiveDate, Vec<CustomDataPoint>>> = {
        let subs: Vec<CustomDataSubscription> = adapter
            .inner
            .lock()
            .unwrap()
            .custom_data_subscriptions
            .clone();
        let mut out: HashMap<String, HashMap<NaiveDate, Vec<CustomDataPoint>>> = HashMap::new();

        for sub in &subs {
            if sub.is_universe() {
                continue;
            }
            let Some(source) = config
                .custom_data_sources
                .iter()
                .find(|s| s.name() == sub.source_type)
            else {
                continue;
            };
            if !source.is_full_history_source() {
                continue;
            }
            let history_path =
                custom_data_history_path(&config.data_root, &sub.source_type, &sub.ticker);

            // Try reading from existing on-disk cache first (synchronous, fast).
            let all_points: Vec<CustomDataPoint> = if history_path.exists() {
                let hp = history_path.clone();
                tokio::task::spawn_blocking(move || {
                    ParquetReader::new()
                        .read_custom_data_points(&hp)
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            } else {
                // Download full series using async HTTP.
                let data_source = match source.get_source(
                    &sub.ticker,
                    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
                    &sub.config,
                ) {
                    Some(s) => s,
                    None => {
                        warn!(
                            "custom data: get_source returned None for {}/{}",
                            sub.source_type, sub.ticker
                        );
                        continue;
                    }
                };
                let raw = match data_source.transport {
                    lean_data::custom::CustomDataTransport::Http => {
                        // Use curl subprocess: handles HTTP/2, TLS quirks, and redirects
                        // more reliably than reqwest in this environment (some servers
                        // like FRED require HTTP/2 which curl negotiates natively).
                        let output = tokio::process::Command::new("curl")
                            .args([
                                "-s",
                                "--max-time",
                                "120",
                                "-L", // follow redirects
                                &data_source.uri,
                            ])
                            .output()
                            .await;
                        match output {
                            Ok(out) if out.status.success() => {
                                String::from_utf8_lossy(&out.stdout).to_string()
                            }
                            Ok(out) => {
                                let stderr = String::from_utf8_lossy(&out.stderr);
                                warn!(
                                    "custom data full-history curl failed for {}/{}: {}",
                                    sub.source_type, sub.ticker, stderr
                                );
                                continue;
                            }
                            Err(e) => {
                                warn!(
                                    "custom data full-history download failed for {}/{}: {}",
                                    sub.source_type, sub.ticker, e
                                );
                                continue;
                            }
                        }
                    }
                    lean_data::custom::CustomDataTransport::LocalFile => {
                        match std::fs::read_to_string(&data_source.uri) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!(
                                    "custom data local file read failed for {}/{}: {}",
                                    sub.source_type, sub.ticker, e
                                );
                                continue;
                            }
                        }
                    }
                };
                // Parse all rows using the plugin (no date filter).
                let source_clone = source.clone();
                let cfg_clone = sub.config.clone();
                let pts: Vec<CustomDataPoint> = tokio::task::spawn_blocking(move || {
                    raw.lines()
                        .filter_map(|line| source_clone.read_history_line(line, &cfg_clone))
                        .collect()
                })
                .await
                .unwrap_or_default();

                if pts.is_empty() {
                    warn!(
                        "custom data: no points parsed for {}/{}",
                        sub.source_type, sub.ticker
                    );
                    continue;
                }
                // Cache to Parquet (off the async thread).
                let hp = history_path.clone();
                let pts_clone = pts.clone();
                tokio::task::spawn_blocking(move || {
                    if let Some(parent) = hp.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    // Disable bloom filters AND page statistics: parquet-rs 53.x
                    // reader panics on TType::Set in metadata written with these
                    // features (read_set_begin is unimplemented). Custom data caches
                    // are always read fully so these features provide no benefit.
                    if let Err(e) = ParquetWriter::new(WriterConfig {
                        bloom_filter: false,
                        write_statistics: false,
                        ..WriterConfig::default()
                    })
                    .write_custom_data_points(&pts_clone, &hp)
                    {
                        warn!("custom data: failed to cache history: {}", e);
                    }
                })
                .await
                .ok();
                pts
            };

            info!(
                "custom data: loaded {} history points for {}/{}",
                all_points.len(),
                sub.source_type,
                sub.ticker
            );
            // Index by date.
            let mut by_date: HashMap<NaiveDate, Vec<CustomDataPoint>> = HashMap::new();
            for pt in all_points {
                by_date.entry(pt.time).or_default().push(pt);
            }
            out.insert(sub.ticker.to_uppercase(), by_date);
        }
        out
    };

    // ── date loop ───────────────────────────────────────────────────────────
    let sim_start = std::time::Instant::now();
    let mut current_date = start_date;
    let mut trading_days = 0i64;
    let mut equity_curve: Vec<Decimal> = Vec::new();
    let mut daily_dates: Vec<String> = Vec::new();

    let finest_resolution = subscriptions
        .iter()
        .map(|s| s.resolution)
        .chain(highest_universe_resolution)
        .min()
        .unwrap_or(Resolution::Daily);
    let resolution_label = match finest_resolution {
        Resolution::Tick => "tick",
        Resolution::Second => "second",
        Resolution::Minute => "minute",
        Resolution::Hour => "hour",
        Resolution::Daily => "daily",
    };
    info!(
        "Backtest: {} → {} ({})",
        start_date, end_date, resolution_label
    );

    while current_date <= end_date {
        if is_intraday {
            // ── INTRADAY LOOP ────────────────────────────────────────────────
            let mut day_trade_bars: HashMap<u64, Vec<TradeBar>> = HashMap::new();
            let mut day_quote_bars: HashMap<u64, Vec<QuoteBar>> = HashMap::new();
            let mut day_ticks: HashMap<u64, Vec<Tick>> = HashMap::new();

            let day_time_params = QueryParams::new().with_time_range(
                date_to_datetime(current_date, 0, 0, 0),
                date_to_datetime(current_date, 23, 59, 59),
            );

            let mut custom_subs: Vec<CustomDataSubscription> = {
                adapter
                    .inner
                    .lock()
                    .unwrap()
                    .custom_data_subscriptions
                    .clone()
            };
            let mut custom_data_for_day = load_low_resolution_custom_data_for_day(
                &custom_subs,
                &custom_history,
                &config,
                current_date,
            )
            .await?;
            let mut universe_data_for_day =
                load_low_resolution_universe_data_for_day(&custom_subs, &config, current_date)
                    .await?;

            let daily_custom_changes = Python::attach(|py| {
                adapter.apply_custom_universe_selection(
                    py,
                    date_to_datetime(current_date, 0, 0, 0).0,
                    Resolution::Daily,
                    &universe_data_for_day,
                )
            });
            if daily_custom_changes.has_changes() {
                let current_subs = { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                let reconciliation = reconcile_runner_subscriptions(
                    &mut subscriptions,
                    &mut loaded_subscription_ids,
                    &current_subs,
                );
                if let Some(ref provider) = config.history_provider {
                    ensure_auxiliary_files_for_subscriptions(
                        provider.clone(),
                        &reconciliation.new_subs,
                        current_date,
                        end_date,
                        &resolver,
                    )
                    .await?;
                    ensure_crypto_future_margin_interest_rates_for_date(
                        provider.clone(),
                        &reconciliation.new_subs,
                        current_date,
                        &resolver,
                    )
                    .await?;
                    ensure_crypto_future_perpetual_contexts_for_date(
                        provider.clone(),
                        &reconciliation.new_subs,
                        current_date,
                        &resolver,
                    )
                    .await?;
                }
                for sub in &reconciliation.new_subs {
                    ensure_map_rows_for_subscription(
                        &factor_reader,
                        &config.data_root,
                        sub,
                        &mut map_file_map,
                        &mut loaded_map_sids,
                    );
                    load_factor_rows_into_map(
                        &factor_reader,
                        &config.data_root,
                        sub,
                        map_file_map.get(&sub.symbol.id.sid).map(Vec::as_slice),
                        current_date,
                        end_date,
                        &mut factor_map,
                        require_factor_files,
                    )?;
                }
                Python::attach(|py| {
                    slice_proxy.retain_subscriptions(py, &current_subs);
                    for sub in &reconciliation.new_subs {
                        let _ = slice_proxy.add_subscription(py, sub);
                    }
                });
                custom_subs = {
                    adapter
                        .inner
                        .lock()
                        .unwrap()
                        .custom_data_subscriptions
                        .clone()
                };
                custom_data_for_day = load_low_resolution_custom_data_for_day(
                    &custom_subs,
                    &custom_history,
                    &config,
                    current_date,
                )
                .await?;
                universe_data_for_day =
                    load_low_resolution_universe_data_for_day(&custom_subs, &config, current_date)
                        .await?;
            }

            let intraday_subs_for_day: Vec<_> = subscriptions
                .iter()
                .filter(|sub| sub.resolution.is_intraday())
                .filter(|sub| is_expected_market_date(&sub.symbol, current_date))
                .filter(|sub| {
                    subscription_has_mapped_data_for_range_cached(
                        &map_file_map,
                        sub,
                        current_date,
                        current_date,
                    )
                })
                .cloned()
                .collect();

            if let Some(ref provider) = config.history_provider {
                let missing_factor_subs: Vec<_> = intraday_subs_for_day
                    .iter()
                    .filter(|sub| matches!(sub.symbol.security_type(), SecurityType::Equity))
                    .filter(|sub| !factor_map.contains_key(&sub.symbol.id.sid))
                    .cloned()
                    .collect();
                if !missing_factor_subs.is_empty() {
                    ensure_auxiliary_files_for_subscriptions(
                        provider.clone(),
                        &missing_factor_subs,
                        current_date,
                        end_date,
                        &resolver,
                    )
                    .await?;
                    for sub in &missing_factor_subs {
                        load_factor_rows_into_map(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            map_file_map.get(&sub.symbol.id.sid).map(Vec::as_slice),
                            current_date,
                            end_date,
                            &mut factor_map,
                            require_factor_files,
                        )?;
                    }
                }
            }

            let day_symbol_sids: Vec<u64> = intraday_subs_for_day
                .iter()
                .map(|sub| sub.symbol.id.sid)
                .collect();
            let day_symbols_by_sid: HashMap<u64, Symbol> = intraday_subs_for_day
                .iter()
                .map(|sub| (sub.symbol.id.sid, sub.symbol.clone()))
                .collect();
            let day_read_params = if day_symbol_sids.is_empty() {
                day_time_params.clone()
            } else {
                day_time_params.clone().with_symbols(day_symbol_sids)
            };

            if let Some(ref provider) = config.history_provider {
                let cache_reader = ParquetReader::new();
                let mut partition_sid_cache: HashMap<PathBuf, HashSet<u64>> = HashMap::new();
                let mut missing_subs = Vec::new();
                for sub in intraday_subs_for_day.iter() {
                    if !cached_partition_has_symbol_sid(
                        &cache_reader,
                        &resolver,
                        sub,
                        current_date,
                        &mut partition_sid_cache,
                    ) {
                        missing_subs.push(sub.clone());
                    }
                }
                if !missing_subs.is_empty() {
                    pre_fetch_high_resolution_day_batched(
                        provider.clone(),
                        config.history_provider.clone(),
                        &missing_subs,
                        current_date,
                        end_date,
                        &resolver,
                    )
                    .await?;
                }
            }

            let mut trade_partition_cache: HashMap<PathBuf, HashMap<u64, Vec<TradeBar>>> =
                HashMap::new();
            let mut quote_partition_cache: HashMap<PathBuf, HashMap<u64, Vec<QuoteBar>>> =
                HashMap::new();
            let mut tick_partition_cache: HashMap<PathBuf, HashMap<u64, Vec<Tick>>> =
                HashMap::new();

            for sub in &intraday_subs_for_day {
                let sub = sub.as_ref();
                let sid = sub.symbol.id.sid;
                let mut had_data = false;

                if sub.resolution == Resolution::Tick {
                    let tick_path = subscription_data_path(&resolver, sub, current_date);
                    if tick_path.exists() {
                        let mut ticks = cached_tick_partition(
                            &mut tick_partition_cache,
                            &reader,
                            tick_path,
                            &sub.symbol,
                            sid,
                            &day_read_params,
                        );
                        if !ticks.is_empty() {
                            ticks.retain(|tick| tick.symbol.id.sid == sid);
                            if let Some(hours) = market_hours_filter.get(&sid) {
                                ticks.retain(|tick| hours.is_open_at(tick.time));
                            }
                            ticks.retain(|tick| match tick.tick_type {
                                TickType::Trade => tick.value > Decimal::ZERO,
                                TickType::Quote => {
                                    tick.bid_price > Decimal::ZERO || tick.ask_price > Decimal::ZERO
                                }
                                TickType::OpenInterest => true,
                            });
                            if !ticks.is_empty() {
                                had_data = true;
                                day_ticks.insert(sid, ticks);
                            }
                        }
                    }
                } else {
                    let trade_path = resolver.market_data_partition(
                        &sub.symbol,
                        sub.resolution,
                        TickType::Trade,
                        current_date,
                    );
                    if trade_path.exists() {
                        let mut bars = cached_trade_partition(
                            &mut trade_partition_cache,
                            &reader,
                            trade_path,
                            &day_symbols_by_sid,
                            sid,
                            &day_read_params,
                        )
                        .await;
                        if !bars.is_empty() {
                            bars.retain(|bar| bar.symbol.id.sid == sid);
                            if let Some(hours) = market_hours_filter.get(&sid) {
                                bars.retain(|bar| hours.is_open_at(bar.time));
                            }
                            bars.retain(|bar| bar.close > Decimal::ZERO);
                            if !bars.is_empty() {
                                had_data = true;
                                day_trade_bars.insert(sid, bars);
                            }
                        }
                    }

                    let quote_path = resolver.market_data_partition(
                        &sub.symbol,
                        sub.resolution,
                        TickType::Quote,
                        current_date,
                    );
                    if quote_path.exists() {
                        let mut bars = cached_quote_partition(
                            &mut quote_partition_cache,
                            &reader,
                            quote_path,
                            &day_symbols_by_sid,
                            sid,
                            &day_read_params,
                        )
                        .await;
                        if !bars.is_empty() {
                            bars.retain(|bar| bar.symbol.id.sid == sid);
                            if let Some(hours) = market_hours_filter.get(&sid) {
                                bars.retain(|bar| hours.is_open_at(bar.time));
                            }
                            bars.retain(|bar| bar.mid_close() > Decimal::ZERO);
                            if !bars.is_empty() {
                                had_data = true;
                                day_quote_bars.insert(sid, bars);
                            }
                        }
                    }
                }

                if had_data {
                    succeeded_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                } else {
                    failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                }
            }

            if let Some(ref provider) = config.history_provider {
                ensure_crypto_future_margin_interest_rates_for_date(
                    provider.clone(),
                    &subscriptions,
                    current_date,
                    &resolver,
                )
                .await?;
                ensure_crypto_future_perpetual_contexts_for_date(
                    provider.clone(),
                    &subscriptions,
                    current_date,
                    &resolver,
                )
                .await?;
            }

            let day_margin_interest_rates = load_margin_interest_rates_for_date(
                reader.as_ref(),
                &resolver,
                &subscriptions,
                current_date,
            )?;
            let day_perpetual_contexts = load_perpetual_contexts_for_date(
                reader.as_ref(),
                &resolver,
                &subscriptions,
                current_date,
            )?;

            let (
                option_subs,
                option_filters,
                option_resolutions,
                open_option_symbols,
            ): OptionRuntimeInputs = {
                let alg = adapter.inner.lock().unwrap();
                (
                    alg.option_subscriptions.clone(),
                    alg.option_filters.clone(),
                    alg.option_subscription_resolutions.clone(),
                    alg.get_option_positions()
                        .into_iter()
                        .map(|position| position.symbol)
                        .collect(),
                )
            };
            let mut option_runtime_requests = Vec::new();
            let option_universe_tickers = option_subs
                .iter()
                .map(|canonical| canonical.permtick.trim_start_matches('?').to_uppercase())
                .collect::<Vec<_>>();
            let option_universes_for_day = load_option_universe_rows_for_tickers(
                &config.data_root,
                &option_universe_tickers,
                current_date,
                config.history_provider.as_ref(),
            )
            .await;
            for canonical in &option_subs {
                let underlying_ticker = canonical.permtick.trim_start_matches('?');
                let option_resolution = option_resolutions
                    .get(&canonical.permtick)
                    .copied()
                    .unwrap_or(Resolution::Minute);
                let spot = {
                    adapter
                        .inner
                        .lock()
                        .unwrap()
                        .securities
                        .all()
                        .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                        .map(|s| s.current_price())
                        .unwrap_or(Decimal::ZERO)
                };
                let ticker = underlying_ticker.to_uppercase();
                let filter = option_filters.get(&canonical.permtick).copied();
                let held_contracts =
                    option_contracts_for_canonical(&open_option_symbols, canonical);
                option_runtime_requests.push(OwnedOptionChainRuntimeRequest {
                    data_root: config.data_root.clone(),
                    ticker,
                    canonical: canonical.clone(),
                    resolution: option_resolution,
                    date: current_date,
                    spot,
                    filter,
                    held_contracts,
                    universe_rows: option_universes_for_day
                        .get(&underlying_ticker.to_uppercase())
                        .cloned(),
                    provider: config.history_provider.clone(),
                });
            }

            let mut option_runtimes: Vec<OptionChainRuntime> = Vec::new();
            for chunk in option_runtime_requests.chunks(OPTION_RUNTIME_PREFETCH_CONCURRENCY) {
                let mut handles = Vec::with_capacity(chunk.len());
                for request in chunk.iter().cloned() {
                    handles.push(tokio::spawn(load_owned_option_chain_runtime(request)));
                }
                for handle in handles {
                    option_runtimes.push(
                        handle
                            .await
                            .context("option chain runtime prefetch task failed")?,
                    );
                }
            }

            let mut high_resolution_custom_by_ts: HashMap<
                i64,
                HashMap<String, Vec<CustomDataPoint>>,
            > = HashMap::new();
            let mut high_resolution_universe_by_ts: HashMap<
                i64,
                HashMap<String, Vec<CustomDataPoint>>,
            > = HashMap::new();
            for sub in custom_subs
                .iter()
                .filter(|sub| sub.config.resolution.is_intraday() && !sub.is_universe())
            {
                let source = config
                    .custom_data_sources
                    .iter()
                    .find(|s| s.name() == sub.source_type)
                    .cloned();
                let points = load_custom_data_points_for_subscription(
                    config.data_root.clone(),
                    sub.source_type.clone(),
                    sub.ticker.clone(),
                    current_date,
                    source,
                    sub.config.clone(),
                    sub.dynamic_query.clone(),
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to load custom data for {}/{} {}",
                        sub.source_type, sub.ticker, current_date
                    )
                })?;
                bucket_high_resolution_custom_points_by_end_time(
                    &mut high_resolution_custom_by_ts,
                    &sub.ticker,
                    points,
                );
            }
            for sub in custom_subs
                .iter()
                .filter(|sub| sub.config.resolution.is_intraday() && sub.is_universe())
            {
                let source = config
                    .custom_data_sources
                    .iter()
                    .find(|s| s.name() == sub.source_type)
                    .cloned();
                let points = load_universe_data_points_for_subscription(
                    config.data_root.clone(),
                    sub.source_type.clone(),
                    sub.ticker.clone(),
                    current_date,
                    source,
                    sub.config.clone(),
                )
                .await
                .with_context(|| {
                    format!(
                        "failed to load universe data for {}/{} {}",
                        sub.source_type, sub.ticker, current_date
                    )
                })?;
                bucket_high_resolution_custom_points_by_end_time(
                    &mut high_resolution_universe_by_ts,
                    &sub.ticker,
                    points,
                );
            }

            let mut all_timestamps: std::collections::BTreeSet<i64> =
                std::collections::BTreeSet::new();
            for bars in day_trade_bars.values() {
                for bar in bars {
                    all_timestamps.insert(bar.time.0);
                }
            }
            for bars in day_quote_bars.values() {
                for bar in bars {
                    all_timestamps.insert(bar.time.0);
                }
            }
            for ticks in day_ticks.values() {
                for tick in ticks {
                    all_timestamps.insert(tick.time.0);
                }
            }
            for rates in day_margin_interest_rates.values() {
                for rate in rates {
                    all_timestamps.insert(rate.time.0);
                }
            }
            for contexts in day_perpetual_contexts.values() {
                for context in contexts {
                    all_timestamps.insert(context.time.0);
                }
            }
            for runtime in &option_runtimes {
                all_timestamps.extend(runtime.timestamps());
            }
            for runtime in &mut option_runtimes {
                if let Some(ts) = runtime.next_tick_time() {
                    all_timestamps.insert(ts);
                }
            }
            all_timestamps.extend(high_resolution_custom_by_ts.keys().copied());
            all_timestamps.extend(high_resolution_universe_by_ts.keys().copied());

            let has_data = !all_timestamps.is_empty();

            if has_data {
                trading_days += 1;
                let first_timestamp = all_timestamps.iter().next().copied();
                let split_time = lean_core::NanosecondTimestamp(
                    first_timestamp.unwrap_or_else(|| date_to_datetime(current_date, 0, 0, 0).0),
                );
                let day_split_events =
                    split_events_for_date(&subscriptions, &factor_map, current_date, split_time);

                let mut trade_by_ts: HashMap<u64, HashMap<i64, TradeBar>> = HashMap::new();
                for (&sid, bars) in &day_trade_bars {
                    let mut ts_map: HashMap<i64, TradeBar> = HashMap::new();
                    for bar in bars {
                        ts_map.insert(bar.time.0, bar.clone());
                    }
                    trade_by_ts.insert(sid, ts_map);
                }

                let mut quote_by_ts: HashMap<u64, HashMap<i64, QuoteBar>> = HashMap::new();
                for (&sid, qbars) in &day_quote_bars {
                    let mut ts_map: HashMap<i64, QuoteBar> = HashMap::new();
                    for q in qbars {
                        ts_map.insert(q.time.0, q.clone());
                    }
                    quote_by_ts.insert(sid, ts_map);
                }
                let mut ticks_by_ts: HashMap<u64, HashMap<i64, Vec<Tick>>> = HashMap::new();
                for (&sid, ticks) in &day_ticks {
                    let mut ts_map: HashMap<i64, Vec<Tick>> = HashMap::new();
                    for tick in ticks {
                        ts_map.entry(tick.time.0).or_default().push(tick.clone());
                    }
                    ticks_by_ts.insert(sid, ts_map);
                }
                let mut margin_interest_by_ts: HashMap<i64, Vec<MarginInterestRate>> =
                    HashMap::new();
                for rates in day_margin_interest_rates.values() {
                    for rate in rates {
                        margin_interest_by_ts
                            .entry(rate.time.0)
                            .or_default()
                            .push(rate.clone());
                    }
                }
                let mut perpetual_context_by_ts: HashMap<i64, Vec<PerpetualContext>> =
                    HashMap::new();
                for contexts in day_perpetual_contexts.values() {
                    for context in contexts {
                        perpetual_context_by_ts
                            .entry(context.time.0)
                            .or_default()
                            .push(context.clone());
                    }
                }

                let mut option_chains_seeded = false;
                let mut daily_universe_seeded = false;
                let mut daily_custom_data_seeded = false;
                while let Some(ts_ns) = all_timestamps.pop_first() {
                    let utc_time = lean_core::NanosecondTimestamp(ts_ns);
                    set_algorithm_time(&adapter, utc_time);

                    let mut minute_slice = Slice::new(utc_time);
                    let mut minute_quote_bars: HashMap<u64, QuoteBar> = HashMap::new();
                    let mut minute_ticks: HashMap<u64, Vec<Tick>> = HashMap::new();
                    let mut bars_for_orders: HashMap<u64, TradeBar> = HashMap::new();

                    if Some(ts_ns) == first_timestamp {
                        let brokerage_name = adapter.inner.lock().unwrap().brokerage_name;
                        apply_split_events_to_state(
                            &day_split_events,
                            &subscriptions,
                            &option_underlying_sids,
                            &portfolio,
                            &order_processor,
                            &mut trade_builder,
                            brokerage_name,
                        );
                        for split in &day_split_events {
                            minute_slice.add_split(split.clone());
                        }
                    }

                    if let Some(rates) = margin_interest_by_ts.get(&ts_ns) {
                        for rate in rates {
                            portfolio.apply_margin_interest_rate(rate);
                            minute_slice.add_margin_interest_rate(rate.clone());
                        }
                    }

                    if let Some(contexts) = perpetual_context_by_ts.get(&ts_ns) {
                        for context in contexts {
                            minute_slice.add_perpetual_context(context.clone());
                        }
                    }

                    for sub in &subscriptions {
                        let sid = sub.symbol.id.sid;
                        if let Some(raw_bar) = trade_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            let bar = if !option_underlying_sids.contains(&sid) {
                                if let Some(rows) = factor_map.get(&sid) {
                                    apply_factor_row(raw_bar.clone(), rows, current_date)
                                } else {
                                    raw_bar.clone()
                                }
                            } else {
                                raw_bar.clone()
                            };
                            adapter
                                .inner
                                .lock()
                                .unwrap()
                                .securities
                                .update_price(&bar.symbol, bar.close);
                            portfolio.update_prices(&bar.symbol, bar.close);
                            bars_for_orders.insert(sid, bar.clone());
                            minute_slice.add_bar(bar);
                        }

                        if let Some(qbar) = quote_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            apply_quote_bar_to_minute(
                                sid,
                                qbar.clone(),
                                current_date,
                                &option_underlying_sids,
                                &factor_map,
                                &mut bars_for_orders,
                                &mut minute_quote_bars,
                                &mut minute_slice,
                                |symbol, bid, ask, mid, update_mid| {
                                    let alg = adapter.inner.lock().unwrap();
                                    if bid > Decimal::ZERO || ask > Decimal::ZERO {
                                        alg.securities.update_quote(symbol, bid, ask);
                                    }
                                    if update_mid {
                                        alg.securities.update_price(symbol, mid);
                                        portfolio.update_prices(symbol, mid);
                                    }
                                },
                            );
                        }

                        if let Some(ticks) = ticks_by_ts.get(&sid).and_then(|m| m.get(&ts_ns)) {
                            if !ticks.is_empty() {
                                let ticks = if !option_underlying_sids.contains(&sid) {
                                    if let Some(rows) = factor_map.get(&sid) {
                                        ticks
                                            .iter()
                                            .cloned()
                                            .map(|tick| apply_factor_tick(tick, rows, current_date))
                                            .collect::<Vec<_>>()
                                    } else {
                                        ticks.to_vec()
                                    }
                                } else {
                                    ticks.to_vec()
                                };
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    bars_for_orders.entry(sid)
                                {
                                    if let Some(synth) = synthesize_trade_bar_from_ticks(
                                        &sub.symbol,
                                        utc_time,
                                        &ticks,
                                    ) {
                                        adapter
                                            .inner
                                            .lock()
                                            .unwrap()
                                            .securities
                                            .update_price(&synth.symbol, synth.close);
                                        portfolio.update_prices(&synth.symbol, synth.close);
                                        e.insert(synth);
                                    }
                                }
                                for tick in &ticks {
                                    minute_slice.add_tick(tick.clone());
                                }
                                minute_ticks.insert(sid, ticks);
                            }
                        }
                    }

                    let mut option_chains_dirty = false;
                    for runtime in &mut option_runtimes {
                        let stream_ticks = runtime.take_stream_ticks_at(ts_ns);
                        if let Some(next_ts) = runtime.next_tick_time() {
                            all_timestamps.insert(next_ts);
                        }
                        let underlying_ticker = runtime.permtick.trim_start_matches('?');
                        let spot = {
                            adapter
                                .inner
                                .lock()
                                .unwrap()
                                .securities
                                .all()
                                .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                                .map(|s| s.current_price())
                                .unwrap_or(Decimal::ZERO)
                        };
                        if runtime.apply_timestamp(utc_time, spot, &stream_ticks) {
                            option_chains_dirty = true;
                        }
                        if let Some(ticks) = runtime.ticks_at(utc_time) {
                            for tick in ticks {
                                minute_slice.add_tick(tick.clone());
                                minute_ticks
                                    .entry(tick.symbol.id.sid)
                                    .or_default()
                                    .push(tick.clone());
                            }
                        }
                        for tick in stream_ticks {
                            minute_slice.add_tick(tick.clone());
                            minute_ticks
                                .entry(tick.symbol.id.sid)
                                .or_default()
                                .push(tick);
                        }
                        let option_order_bars: Vec<TradeBar> = runtime
                            .chain
                            .contracts
                            .values()
                            .filter_map(|contract| {
                                synthesize_trade_bar_from_option_contract(contract, utc_time)
                            })
                            .collect();
                        if !option_order_bars.is_empty() {
                            let alg = adapter.inner.lock().unwrap();
                            for bar in option_order_bars {
                                let sid = bar.symbol.id.sid;
                                alg.securities.update_price(&bar.symbol, bar.close);
                                portfolio.update_prices(&bar.symbol, bar.close);
                                bars_for_orders.entry(sid).or_insert(bar);
                            }
                        }
                    }

                    let chains_snapshot = if !option_runtimes.is_empty()
                        && (!option_chains_seeded || option_chains_dirty)
                    {
                        option_chains_seeded = true;
                        let chains_snapshot: Vec<(&str, &OptionChain)> = option_runtimes
                            .iter()
                            .map(|runtime| (runtime.permtick.as_str(), &runtime.chain))
                            .collect();
                        {
                            let mut alg = adapter.inner.lock().unwrap();
                            update_option_chain_map_in_place(
                                &mut alg.option_chains,
                                &chains_snapshot,
                            );
                        }
                        sync_option_holdings_to_chain_prices(
                            &adapter,
                            &portfolio,
                            &chains_snapshot,
                        );
                        Some(chains_snapshot)
                    } else {
                        None
                    };

                    Python::attach(|py| {
                        adapter.apply_universe_selection(py, utc_time.0, finest_resolution);
                    });

                    let mut universe_selection_resolutions: Vec<Resolution> = Vec::new();
                    let universe_data_for_slice = if let Some(points_by_ticker) =
                        high_resolution_universe_by_ts.get(&utc_time.0)
                    {
                        let data = points_by_ticker.clone();
                        for ticker in data.keys() {
                            for sub in custom_subs
                                .iter()
                                .filter(|sub| sub.is_universe())
                                .filter(|sub| sub.ticker.eq_ignore_ascii_case(ticker))
                            {
                                if !universe_selection_resolutions.contains(&sub.config.resolution)
                                {
                                    universe_selection_resolutions.push(sub.config.resolution);
                                }
                            }
                        }
                        Some(data)
                    } else if !daily_universe_seeded {
                        daily_universe_seeded = true;
                        universe_selection_resolutions.push(Resolution::Daily);
                        Some(universe_data_for_day.clone())
                    } else {
                        None
                    };

                    if let Some(universe_data_for_slice) = universe_data_for_slice.as_ref() {
                        Python::attach(|py| {
                            for universe_resolution in universe_selection_resolutions {
                                adapter.apply_custom_universe_selection(
                                    py,
                                    utc_time.0,
                                    universe_resolution,
                                    universe_data_for_slice,
                                );
                            }
                        });
                    }

                    let custom_data_for_slice = if let Some(points_by_ticker) =
                        high_resolution_custom_by_ts.get(&utc_time.0)
                    {
                        let mut merged = custom_data_for_day.clone();
                        for (ticker, points) in points_by_ticker {
                            merged.insert(ticker.clone(), points.clone());
                        }
                        Some(merged)
                    } else if !daily_custom_data_seeded && !custom_data_for_day.is_empty() {
                        daily_custom_data_seeded = true;
                        Some(custom_data_for_day.clone())
                    } else {
                        None
                    };

                    if let Some(custom_data_for_slice) = custom_data_for_slice.as_ref() {
                        minute_slice.custom_data = custom_data_for_slice.clone();
                        minute_slice.has_data = true;
                    }

                    // Mirror C# LEAN's DataManager subscription lifecycle:
                    // additions are attached to the active stream, while
                    // universe removals are pruned from runner-local state so
                    // later days don't fetch stale parquet for removed symbols.
                    let current_subs =
                        { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                    let reconciliation = reconcile_runner_subscriptions(
                        &mut subscriptions,
                        &mut loaded_subscription_ids,
                        &current_subs,
                    );
                    let removed_sids: Vec<u64> = reconciliation
                        .removed_subs
                        .iter()
                        .map(|sub| sub.symbol.id.sid)
                        .collect();
                    if !removed_sids.is_empty() {
                        debug!(
                            "Pruned {} inactive universe subscriptions",
                            removed_sids.len()
                        );
                        for sid in &removed_sids {
                            minute_slice.bars.remove(sid);
                            minute_slice.quote_bars.remove(sid);
                            minute_slice.ticks.remove(sid);
                            minute_slice.margin_interest_rates.remove(sid);
                            minute_quote_bars.remove(sid);
                            minute_ticks.remove(sid);
                            bars_for_orders.remove(sid);
                        }
                        minute_slice.has_data = !minute_slice.bars.is_empty()
                            || !minute_slice.quote_bars.is_empty()
                            || !minute_slice.ticks.is_empty()
                            || !minute_slice.margin_interest_rates.is_empty()
                            || !minute_slice.dividends.is_empty()
                            || !minute_slice.splits.is_empty()
                            || !minute_slice.delistings.is_empty()
                            || !minute_slice.symbol_changed_events.is_empty();
                    }
                    Python::attach(|py| {
                        slice_proxy.retain_subscriptions(py, &current_subs);
                        for sub in &reconciliation.new_subs {
                            let _ = slice_proxy.add_subscription(py, sub);
                        }
                    });
                    let new_subs = reconciliation.new_subs;

                    let new_high_res_subs: Vec<_> = new_subs
                        .iter()
                        .filter(|sub| sub.resolution.is_intraday())
                        .cloned()
                        .collect();
                    if !new_high_res_subs.is_empty() {
                        if let Some(ref provider) = config.history_provider {
                            pre_fetch_high_resolution_day_batched(
                                provider.clone(),
                                config.history_provider.clone(),
                                &new_high_res_subs,
                                current_date,
                                end_date,
                                &resolver,
                            )
                            .await?;
                        }
                    }

                    if let Some(ref provider) = config.history_provider {
                        ensure_auxiliary_files_for_subscriptions(
                            provider.clone(),
                            &new_subs,
                            current_date,
                            end_date,
                            &resolver,
                        )
                        .await?;
                        ensure_crypto_future_margin_interest_rates_for_date(
                            provider.clone(),
                            &new_subs,
                            current_date,
                            &resolver,
                        )
                        .await?;
                        ensure_crypto_future_perpetual_contexts_for_date(
                            provider.clone(),
                            &new_subs,
                            current_date,
                            &resolver,
                        )
                        .await?;
                    }

                    for sub in &new_subs {
                        let sid = sub.symbol.id.sid;
                        if !sub.resolution.is_intraday() {
                            continue;
                        }

                        ensure_map_rows_for_subscription(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            &mut map_file_map,
                            &mut loaded_map_sids,
                        );
                        load_factor_rows_into_map(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            map_file_map.get(&sub.symbol.id.sid).map(Vec::as_slice),
                            current_date,
                            end_date,
                            &mut factor_map,
                            require_factor_files,
                        )?;

                        let exchange_hours = {
                            adapter
                                .inner
                                .lock()
                                .unwrap()
                                .securities
                                .get(&sub.symbol)
                                .map(|s| s.exchange_hours.clone())
                        };

                        if sub.resolution == Resolution::Tick {
                            let tick_path = subscription_data_path(&resolver, sub, current_date);
                            if tick_path.exists() {
                                match reader.read_tick_partition(
                                    &tick_path,
                                    &sub.symbol,
                                    &day_time_params.clone().with_symbols(vec![sid]),
                                ) {
                                    Ok(mut ticks) if !ticks.is_empty() => {
                                        ticks.retain(|tick| tick.symbol.id.sid == sid);
                                        if let Some(hours) = &exchange_hours {
                                            ticks.retain(|tick| hours.is_open_at(tick.time));
                                        }
                                        ticks.retain(|tick| match tick.tick_type {
                                            TickType::Trade => tick.value > Decimal::ZERO,
                                            TickType::Quote => {
                                                tick.bid_price > Decimal::ZERO
                                                    || tick.ask_price > Decimal::ZERO
                                            }
                                            TickType::OpenInterest => true,
                                        });
                                        if !ticks.is_empty() {
                                            let mut ts_map: HashMap<i64, Vec<Tick>> =
                                                HashMap::new();
                                            for tick in &ticks {
                                                ts_map
                                                    .entry(tick.time.0)
                                                    .or_default()
                                                    .push(tick.clone());
                                            }
                                            ticks_by_ts.insert(sid, ts_map);
                                            day_ticks.insert(sid, ticks);
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(
                                        "Failed to read dynamic ticks for {} on {}: {}",
                                        sub.symbol.value, current_date, e
                                    ),
                                }
                            }
                        } else {
                            let dynamic_symbols_by_sid = HashMap::from([(sid, sub.symbol.clone())]);
                            let trade_path = resolver.market_data_partition(
                                &sub.symbol,
                                sub.resolution,
                                TickType::Trade,
                                current_date,
                            );
                            if trade_path.exists() {
                                match reader
                                    .read_trade_bar_partition_grouped_async(
                                        &trade_path,
                                        &dynamic_symbols_by_sid,
                                        &day_time_params.clone().with_symbols(vec![sid]),
                                    )
                                    .await
                                    .map(|mut grouped| grouped.remove(&sid).unwrap_or_default())
                                {
                                    Ok(mut bars) if !bars.is_empty() => {
                                        bars.retain(|bar| bar.symbol.id.sid == sid);
                                        if let Some(hours) = &exchange_hours {
                                            bars.retain(|bar| hours.is_open_at(bar.time));
                                        }
                                        bars.retain(|bar| bar.close > Decimal::ZERO);
                                        if !bars.is_empty() {
                                            let mut ts_map: HashMap<i64, TradeBar> = HashMap::new();
                                            for bar in &bars {
                                                ts_map.insert(bar.time.0, bar.clone());
                                            }
                                            if let Some(raw_bar) = ts_map.get(&utc_time.0).cloned()
                                            {
                                                let bar = if !option_underlying_sids.contains(&sid)
                                                {
                                                    if let Some(rows) = factor_map.get(&sid) {
                                                        apply_factor_row(
                                                            raw_bar,
                                                            rows,
                                                            current_date,
                                                        )
                                                    } else {
                                                        raw_bar
                                                    }
                                                } else {
                                                    raw_bar
                                                };
                                                adapter
                                                    .inner
                                                    .lock()
                                                    .unwrap()
                                                    .securities
                                                    .update_price(&bar.symbol, bar.close);
                                                portfolio.update_prices(&bar.symbol, bar.close);
                                                bars_for_orders.insert(sid, bar.clone());
                                                minute_slice.add_bar(bar);
                                            }
                                            trade_by_ts.insert(sid, ts_map);
                                            day_trade_bars.insert(sid, bars);
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(
                                        "Failed to read dynamic intraday bars for {} on {}: {}",
                                        sub.symbol.value, current_date, e
                                    ),
                                }
                            }

                            let quote_path = resolver.market_data_partition(
                                &sub.symbol,
                                sub.resolution,
                                TickType::Quote,
                                current_date,
                            );
                            if quote_path.exists() {
                                match reader
                                    .read_quote_bar_partition_grouped_async(
                                        &quote_path,
                                        &dynamic_symbols_by_sid,
                                        &day_time_params.clone().with_symbols(vec![sid]),
                                    )
                                    .await
                                    .map(|mut grouped| grouped.remove(&sid).unwrap_or_default())
                                {
                                    Ok(mut bars) if !bars.is_empty() => {
                                        bars.retain(|bar| bar.symbol.id.sid == sid);
                                        if let Some(hours) = &exchange_hours {
                                            bars.retain(|bar| hours.is_open_at(bar.time));
                                        }
                                        bars.retain(|bar| bar.mid_close() > Decimal::ZERO);
                                        if !bars.is_empty() {
                                            let mut ts_map: HashMap<i64, QuoteBar> = HashMap::new();
                                            for qbar in &bars {
                                                ts_map.insert(qbar.time.0, qbar.clone());
                                            }
                                            quote_by_ts.insert(sid, ts_map);
                                            day_quote_bars.insert(sid, bars);
                                            if let Some(qbar) = quote_by_ts
                                                .get(&sid)
                                                .and_then(|m| m.get(&utc_time.0).cloned())
                                            {
                                                apply_quote_bar_to_minute(
                                                    sid,
                                                    qbar,
                                                    current_date,
                                                    &option_underlying_sids,
                                                    &factor_map,
                                                    &mut bars_for_orders,
                                                    &mut minute_quote_bars,
                                                    &mut minute_slice,
                                                    |symbol, bid, ask, mid, update_mid| {
                                                        let alg = adapter.inner.lock().unwrap();
                                                        if bid > Decimal::ZERO
                                                            || ask > Decimal::ZERO
                                                        {
                                                            alg.securities
                                                                .update_quote(symbol, bid, ask);
                                                        }
                                                        if update_mid {
                                                            alg.securities
                                                                .update_price(symbol, mid);
                                                            portfolio.update_prices(symbol, mid);
                                                        }
                                                    },
                                                );
                                            }
                                        }
                                    }
                                    Ok(_) => {}
                                    Err(e) => warn!(
                                        "Failed to read dynamic quote bars for {} on {}: {}",
                                        sub.symbol.value, current_date, e
                                    ),
                                }
                            }
                        }
                    }

                    // Process already-open orders before the algorithm sees this
                    // slice. This matches LEAN's event ordering for pending
                    // market orders carried into a new bar: portfolio state is
                    // updated from executable market data before alpha/portfolio
                    // construction can emit new targets.
                    let mut pre_existing_fill_events = order_processor
                        .generate_order_events_with_quotes(
                            &bars_for_orders,
                            &minute_quote_bars,
                            utc_time,
                        );
                    OrderEventProcessingContext {
                        adapter: &mut adapter,
                        portfolio: &portfolio,
                        order_processor: &order_processor,
                        all_order_events: &mut all_order_events,
                        trade_builder: &mut trade_builder,
                        completed_trades: &mut completed_trades,
                        live_writer: as_sidecar_writer(live_writer.as_ref()),
                    }
                    .process(&mut pre_existing_fill_events);

                    Python::attach(|py| {
                        if let Some(chains_snapshot) = chains_snapshot.as_ref() {
                            slice_proxy.update_option_chains(py, chains_snapshot);
                        }
                        slice_proxy.update_quote_bars(py, &minute_quote_bars);
                        slice_proxy.update_margin_interest_rates(py, &minute_slice);
                        slice_proxy.update_perpetual_contexts(py, &minute_slice);
                        slice_proxy.update_ticks(py, &minute_ticks);
                        if let Some(custom_data_for_slice) = custom_data_for_slice.as_ref() {
                            slice_proxy.update_custom_data(py, custom_data_for_slice);
                        }
                        adapter.on_data_proxy(py, &slice_proxy, &minute_slice);
                    });

                    // ── Algorithm Framework pipeline (intraday) ───────────
                    {
                        let order_requests = run_framework_pipeline(
                            &adapter.framework,
                            &adapter.inner,
                            &minute_slice,
                        );
                        if !order_requests.is_empty() {
                            let mut alg = adapter.inner.lock().unwrap();
                            for req in order_requests {
                                submit_execution_order_request(&mut alg, req);
                            }
                        }
                    }

                    let mut update_events =
                        drain_local_update_requests(&order_processor, utc_time, "backtest");
                    OrderEventProcessingContext {
                        adapter: &mut adapter,
                        portfolio: &portfolio,
                        order_processor: &order_processor,
                        all_order_events: &mut all_order_events,
                        trade_builder: &mut trade_builder,
                        completed_trades: &mut completed_trades,
                        live_writer: as_sidecar_writer(live_writer.as_ref()),
                    }
                    .process(&mut update_events);

                    let mut cancel_events =
                        drain_local_cancel_requests(&order_processor, utc_time, "backtest");
                    OrderEventProcessingContext {
                        adapter: &mut adapter,
                        portfolio: &portfolio,
                        order_processor: &order_processor,
                        all_order_events: &mut all_order_events,
                        trade_builder: &mut trade_builder,
                        completed_trades: &mut completed_trades,
                        live_writer: as_sidecar_writer(live_writer.as_ref()),
                    }
                    .process(&mut cancel_events);

                    let mut fill_events = order_processor.generate_order_events_with_quotes(
                        &bars_for_orders,
                        &minute_quote_bars,
                        utc_time,
                    );
                    OrderEventProcessingContext {
                        adapter: &mut adapter,
                        portfolio: &portfolio,
                        order_processor: &order_processor,
                        all_order_events: &mut all_order_events,
                        trade_builder: &mut trade_builder,
                        completed_trades: &mut completed_trades,
                        live_writer: as_sidecar_writer(live_writer.as_ref()),
                    }
                    .process(&mut fill_events);
                }

                // End-of-day calls.
                adapter.on_end_of_day(None);
                process_option_expirations(&mut adapter, current_date, &HashMap::new());

                let bm_close: Option<Decimal> = if benchmark_in_subs {
                    day_trade_bars
                        .get(&benchmark_sid)
                        .and_then(|bars| bars.last())
                        .map(|b| b.close)
                        .or_else(|| {
                            day_quote_bars
                                .get(&benchmark_sid)
                                .and_then(|bars| bars.last())
                                .map(|bar| bar.mid_close())
                        })
                        .or_else(|| {
                            day_ticks.get(&benchmark_sid).and_then(|ticks| {
                                ticks.iter().rev().find_map(|tick| match tick.tick_type {
                                    TickType::Trade if tick.value > Decimal::ZERO => {
                                        Some(tick.value)
                                    }
                                    TickType::Quote if tick.value > Decimal::ZERO => {
                                        Some(tick.value)
                                    }
                                    _ => None,
                                })
                            })
                        })
                } else {
                    benchmark_price_map.get(&current_date).copied()
                };
                if let Some(close) = bm_close {
                    benchmark_curve.push(close);
                    benchmark_dates.push(current_date.to_string());
                }

                let day_equity = portfolio.total_portfolio_value();
                equity_curve.push(day_equity);
                daily_dates.push(current_date.to_string());
                if let Some(writer) = &mut live_writer {
                    writer.record_progress(
                        current_date,
                        trading_days,
                        day_equity,
                        all_order_events.len(),
                        completed_trades.len(),
                    );
                }
            }

            current_date += chrono::Duration::days(1);
        } else {
            // ── DAILY LOOP ───────────────────────────────────────────────────

            // ── lazy-load bars for dynamically added subscriptions ────────────
            // Strategies may call add_equity() mid-backtest (universe selection).
            // Detect new subscriptions and load their full bar history so that
            // security prices are available when set_holdings() is called.
            if !is_intraday {
                let current_subs = { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                for sub in &current_subs {
                    let sid = sub.symbol.id.sid;
                    if !loaded_subscription_ids.contains(&sub.unique_id()) {
                        loaded_subscription_ids.insert(sub.unique_id());
                        let path = subscription_data_path(&resolver, sub, start_date);
                        // If the bar file isn't cached locally, fetch it now from
                        // the historical provider (same as pre_fetch_all does at startup).
                        if !path.exists() {
                            if let Some(ref provider) = config.historical_provider {
                                pre_fetch_all(
                                    provider.clone(),
                                    config.history_provider.clone(),
                                    std::slice::from_ref(sub),
                                    start_date,
                                    end_date,
                                    &resolver,
                                )
                                .await?;
                            }
                        }
                        if let Some(ref provider) = config.history_provider {
                            ensure_auxiliary_files_for_subscriptions(
                                provider.clone(),
                                std::slice::from_ref(sub),
                                start_date,
                                end_date,
                                &resolver,
                            )
                            .await?;
                            ensure_crypto_future_margin_interest_rates_for_date(
                                provider.clone(),
                                std::slice::from_ref(sub),
                                current_date,
                                &resolver,
                            )
                            .await?;
                            ensure_crypto_future_perpetual_contexts_for_date(
                                provider.clone(),
                                std::slice::from_ref(sub),
                                current_date,
                                &resolver,
                            )
                            .await?;
                        }
                        ensure_map_rows_for_subscription(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            &mut map_file_map,
                            &mut loaded_map_sids,
                        );
                        load_factor_rows_into_map(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            map_file_map.get(&sub.symbol.id.sid).map(Vec::as_slice),
                            start_date,
                            end_date,
                            &mut factor_map,
                            require_factor_files,
                        )?;
                        if path.exists() {
                            let bars = load_trade_bar_partitions(
                                &reader,
                                &resolver,
                                sub,
                                start_date,
                                end_date,
                                &daily_full_params,
                            );
                            let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> =
                                bars.into_iter().map(|b| (b.time.date_utc(), b)).collect();
                            if !date_map.is_empty() {
                                bar_map.insert(sid, date_map);
                            }
                        }
                        subscriptions.push(sub.clone());
                    }
                }
            }

            let utc_time = date_to_datetime(current_date, 16, 0, 0);
            set_algorithm_time(&adapter, utc_time);

            Python::attach(|py| {
                adapter.apply_universe_selection(py, utc_time.0, Resolution::Daily);
            });

            let custom_subs_for_universe: Vec<CustomDataSubscription> = {
                adapter
                    .inner
                    .lock()
                    .unwrap()
                    .custom_data_subscriptions
                    .clone()
            };
            let universe_data_for_day = load_low_resolution_universe_data_for_day(
                &custom_subs_for_universe,
                &config,
                current_date,
            )
            .await?;
            if !universe_data_for_day.is_empty() {
                Python::attach(|py| {
                    adapter.apply_custom_universe_selection(
                        py,
                        utc_time.0,
                        Resolution::Daily,
                        &universe_data_for_day,
                    );
                });
            }

            // Universe selection can add subscriptions at the frontier before
            // OnData. Load those subscriptions now so the current slice can
            // contain their data, matching C# LEAN's time-pulse selection flow.
            if !is_intraday {
                let current_subs = { adapter.inner.lock().unwrap().subscription_manager.get_all() };
                for sub in &current_subs {
                    let sid = sub.symbol.id.sid;
                    if !loaded_subscription_ids.contains(&sub.unique_id()) {
                        loaded_subscription_ids.insert(sub.unique_id());
                        let path = subscription_data_path(&resolver, sub, start_date);
                        if !path.exists() {
                            if let Some(ref provider) = config.historical_provider {
                                pre_fetch_all(
                                    provider.clone(),
                                    config.history_provider.clone(),
                                    std::slice::from_ref(sub),
                                    start_date,
                                    end_date,
                                    &resolver,
                                )
                                .await?;
                            }
                        }
                        if let Some(ref provider) = config.history_provider {
                            ensure_auxiliary_files_for_subscriptions(
                                provider.clone(),
                                std::slice::from_ref(sub),
                                start_date,
                                end_date,
                                &resolver,
                            )
                            .await?;
                            ensure_crypto_future_margin_interest_rates_for_date(
                                provider.clone(),
                                std::slice::from_ref(sub),
                                current_date,
                                &resolver,
                            )
                            .await?;
                            ensure_crypto_future_perpetual_contexts_for_date(
                                provider.clone(),
                                std::slice::from_ref(sub),
                                current_date,
                                &resolver,
                            )
                            .await?;
                        }
                        ensure_map_rows_for_subscription(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            &mut map_file_map,
                            &mut loaded_map_sids,
                        );
                        load_factor_rows_into_map(
                            &factor_reader,
                            &config.data_root,
                            sub,
                            map_file_map.get(&sub.symbol.id.sid).map(Vec::as_slice),
                            start_date,
                            end_date,
                            &mut factor_map,
                            require_factor_files,
                        )?;
                        if path.exists() {
                            let bars = load_trade_bar_partitions(
                                &reader,
                                &resolver,
                                sub,
                                start_date,
                                end_date,
                                &daily_full_params,
                            );
                            let date_map: HashMap<chrono::NaiveDate, lean_data::TradeBar> =
                                bars.into_iter().map(|b| (b.time.date_utc(), b)).collect();
                            if !date_map.is_empty() {
                                bar_map.insert(sid, date_map);
                            }
                        }
                        subscriptions.push(sub.clone());
                        let _ = Python::attach(|py| slice_proxy.add_subscription(py, sub));
                    }
                }
            }

            // Split events are generated from factor-file boundaries for every
            // equity subscription.  C# LEAN applies holdings/open-order/trade
            // adjustments only in live/raw mode; option underlyings are treated
            // as raw here because their bars intentionally bypass factor scaling.
            let day_split_events =
                split_events_for_date(&subscriptions, &factor_map, current_date, utc_time);
            let brokerage_name = adapter.inner.lock().unwrap().brokerage_name;
            apply_split_events_to_state(
                &day_split_events,
                &subscriptions,
                &option_underlying_sids,
                &portfolio,
                &order_processor,
                &mut trade_builder,
                brokerage_name,
            );
            let split_ratios_today: HashMap<u64, f64> = day_split_events
                .iter()
                .filter(|split| option_underlying_sids.contains(&split.symbol.id.sid))
                .filter_map(|split| {
                    let sf = split.split_factor.to_f64()?;
                    (sf != 0.0).then_some((split.symbol.id.sid, 1.0 / sf))
                })
                .collect();

            let mut slice = Slice::new(utc_time);
            for split in &day_split_events {
                slice.add_split(split.clone());
            }
            for sub in &subscriptions {
                let sid = sub.symbol.id.sid;
                if let Some(day_bar) = bar_map.get(&sid).and_then(|m| m.get(&current_date)) {
                    let bar = if !option_underlying_sids.contains(&sid) {
                        if let Some(rows) = factor_map.get(&sid) {
                            apply_factor_row(day_bar.clone(), rows, current_date)
                        } else {
                            day_bar.clone()
                        }
                    } else {
                        day_bar.clone()
                    };
                    adapter
                        .inner
                        .lock()
                        .unwrap()
                        .securities
                        .update_price(&bar.symbol, bar.close);
                    portfolio.update_prices(&bar.symbol, bar.close);
                    succeeded_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                    slice.add_bar(bar);
                } else {
                    failed_data_requests.push(format!("{}/{}", sub.symbol.value, current_date));
                }
            }

            if let Some(ref provider) = config.history_provider {
                ensure_crypto_future_margin_interest_rates_for_date(
                    provider.clone(),
                    &subscriptions,
                    current_date,
                    &resolver,
                )
                .await?;
            }

            let day_margin_interest_rates = load_margin_interest_rates_for_date(
                reader.as_ref(),
                &resolver,
                &subscriptions,
                current_date,
            )?;
            for rates in day_margin_interest_rates.values() {
                for rate in rates {
                    portfolio.apply_margin_interest_rate(rate);
                    slice.add_margin_interest_rate(rate.clone());
                }
            }

            if !slice.has_data {
                current_date += chrono::Duration::days(1);
                continue;
            }

            // Record benchmark close for this trading day.
            let bm_close: Option<Decimal> = if benchmark_in_subs {
                slice.bars.get(&benchmark_sid).map(|b| b.close)
            } else {
                benchmark_price_map.get(&current_date).copied()
            };
            if let Some(close) = bm_close {
                benchmark_curve.push(close);
                benchmark_dates.push(current_date.to_string());
            }

            trading_days += 1;

            // Build option chains before calling on_data.
            let pending_option_order_sids: HashSet<u64> = order_processor
                .transaction_manager
                .get_open_orders()
                .into_iter()
                .filter(|order| order.symbol.option_symbol_id().is_some())
                .map(|order| order.symbol.id.sid)
                .collect();
            let mut option_order_bars: HashMap<u64, lean_data::TradeBar> = HashMap::new();
            let mut option_eod_bars_for_day: HashMap<String, (Symbol, Vec<OptionEodBar>)> =
                HashMap::new();
            {
                let (option_subs, option_filters, open_option_symbols): (
                    Vec<Symbol>,
                    HashMap<String, OptionFilter>,
                    Vec<Symbol>,
                ) = {
                    let alg = adapter.inner.lock().unwrap();
                    (
                        alg.option_subscriptions.clone(),
                        alg.option_filters.clone(),
                        alg.get_option_positions()
                            .into_iter()
                            .map(|position| position.symbol)
                            .collect(),
                    )
                };

                let mut chains_for_day: Vec<(String, OptionChain)> = Vec::new();
                for canonical in &option_subs {
                    let underlying_ticker = canonical.permtick.trim_start_matches('?');
                    let underlying_sym: Symbol = canonical
                        .underlying
                        .as_ref()
                        .map(|u| *u.clone())
                        .unwrap_or_else(|| canonical.clone());
                    let spot = {
                        adapter
                            .inner
                            .lock()
                            .unwrap()
                            .securities
                            .all()
                            .find(|s| s.symbol.permtick.eq_ignore_ascii_case(underlying_ticker))
                            .map(|s| s.current_price())
                            .unwrap_or(Decimal::ZERO)
                    };

                    let ticker = underlying_ticker.to_uppercase();
                    let bars = load_option_eod_bars(
                        &config.data_root,
                        &ticker,
                        current_date,
                        config.history_provider.as_ref(),
                    )
                    .await;
                    if !bars.is_empty() {
                        option_eod_bars_for_day.insert(
                            canonical.permtick.clone(),
                            (underlying_sym.clone(), bars.clone()),
                        );
                    }
                    if !pending_option_order_sids.is_empty() {
                        for bar in &bars {
                            let Some(symbol) = option_eod_bar_symbol(bar, &underlying_sym) else {
                                continue;
                            };
                            if pending_option_order_sids.contains(&symbol.id.sid) {
                                if let Some(order_bar) =
                                    option_eod_bar_to_order_trade_bar(bar, symbol, utc_time)
                                {
                                    option_order_bars.insert(order_bar.symbol.id.sid, order_bar);
                                }
                            }
                        }
                    };
                    let chain = if !bars.is_empty() {
                        let held_contracts =
                            option_contracts_for_canonical(&open_option_symbols, canonical);
                        build_option_chain_from_eod_bars(
                            canonical,
                            spot,
                            utc_time,
                            &bars,
                            option_filters.get(&canonical.permtick).copied(),
                            &held_contracts,
                        )
                    } else {
                        OptionChain::new(canonical.clone(), spot)
                    };
                    chains_for_day.push((canonical.permtick.clone(), chain));
                }

                let mut alg = adapter.inner.lock().unwrap();
                for (permtick, chain) in chains_for_day {
                    alg.option_chains.insert(permtick, chain);
                }
            }

            let chains_snapshot: Vec<(String, OptionChain)> = {
                let alg = adapter.inner.lock().unwrap();
                alg.option_chains
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            let chains_snapshot_refs: Vec<(&str, &OptionChain)> = chains_snapshot
                .iter()
                .map(|(permtick, chain)| (permtick.as_str(), chain))
                .collect();
            sync_option_holdings_to_chain_prices(&adapter, &portfolio, &chains_snapshot_refs);

            let mut bars_map: HashMap<u64, lean_data::TradeBar> =
                slice.bars.iter().map(|(&k, v)| (k, v.clone())).collect();
            bars_map.extend(option_order_bars);

            let mut fill_events = order_processor.generate_order_events(&bars_map, utc_time);
            OrderEventProcessingContext {
                adapter: &mut adapter,
                portfolio: &portfolio,
                order_processor: &order_processor,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
                live_writer: as_sidecar_writer(live_writer.as_ref()),
            }
            .process(&mut fill_events);

            // ── Custom data fetch (daily) ─────────────────────────────────
            let custom_subs: Vec<CustomDataSubscription> = {
                adapter
                    .inner
                    .lock()
                    .unwrap()
                    .custom_data_subscriptions
                    .clone()
            };
            let mut custom_data_for_day: HashMap<String, Vec<CustomDataPoint>> = HashMap::new();
            for sub in custom_subs.iter().filter(|sub| !sub.is_universe()) {
                let key = sub.ticker.to_uppercase();
                if let Some(by_date) = custom_history.get(&key) {
                    // Full-history source: look up from preloaded in-memory map.
                    if let Some(pts) = by_date.get(&current_date) {
                        custom_data_for_day.insert(sub.ticker.clone(), pts.clone());
                    }
                } else {
                    // Date-keyed source: per-day HTTP fetch with per-day Parquet cache.
                    let source = config
                        .custom_data_sources
                        .iter()
                        .find(|s| s.name() == sub.source_type)
                        .cloned();
                    let data_root = config.data_root.clone();
                    let source_type = sub.source_type.clone();
                    let ticker = sub.ticker.clone();
                    let cfg = sub.config.clone();
                    let dynamic_query = sub.dynamic_query.clone();
                    let points = load_custom_data_points_for_subscription(
                        data_root,
                        source_type,
                        ticker,
                        current_date,
                        source,
                        cfg,
                        dynamic_query,
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "failed to load custom data for {}/{} {}",
                            sub.source_type, sub.ticker, current_date
                        )
                    })?;
                    if !points.is_empty() {
                        custom_data_for_day.insert(sub.ticker.clone(), points);
                    }
                }
            }
            if !custom_data_for_day.is_empty() {
                slice.custom_data = custom_data_for_day.clone();
                slice.has_data = true;
            }

            // ── Map file: check for ticker renames and delistings ────────────
            for sub in &subscriptions {
                let sid = sub.symbol.id.sid;
                let Some(rows) = map_file_map.get(&sid) else {
                    continue;
                };

                // Check for rename (ticker change between yesterday and today)
                let today_ticker = ticker_at_date(rows, current_date);
                let yesterday_ticker =
                    ticker_at_date(rows, current_date - chrono::Duration::days(1));
                if let (Some(old), Some(new)) = (yesterday_ticker, today_ticker) {
                    if old != new {
                        let ev = SymbolChangedEvent::new(
                            sub.symbol.clone(),
                            utc_time,
                            old.to_string(),
                            new.to_string(),
                        );
                        slice.add_symbol_changed(ev);
                        info!("Symbol rename: {} → {} on {}", old, new, current_date);
                    }
                }

                // Check for delisting
                if let Some(delist_date) = delisting_date(rows) {
                    let last_price = portfolio.get_holding(&sub.symbol).last_price;
                    if current_date == delist_date {
                        // Warning: last day of trading
                        slice.add_delisting(Delisting::new(
                            sub.symbol.clone(),
                            utc_time,
                            last_price,
                            DelistingType::Warning,
                        ));
                        // Auto-liquidate: place market order to close position
                        if portfolio.is_invested(&sub.symbol) {
                            let holding = portfolio.get_holding(&sub.symbol);
                            let qty = -holding.quantity;
                            adapter.inner.lock().unwrap().market_order(&sub.symbol, qty);
                        }
                    } else if current_date == delist_date + chrono::Duration::days(1) {
                        slice.add_delisting(Delisting::new(
                            sub.symbol.clone(),
                            utc_time,
                            last_price,
                            DelistingType::Delisted,
                        ));
                    }
                }
            }

            Python::attach(|py| {
                slice_proxy.update_option_chains(py, &chains_snapshot_refs);
                slice_proxy.update_quote_bars(py, &HashMap::new());
                slice_proxy.update_perpetual_contexts(py, &slice);
                slice_proxy.update_ticks(py, &HashMap::new());
                slice_proxy.update_custom_data(py, &custom_data_for_day);
                adapter.on_data_proxy(py, &slice_proxy, &slice);

                // Fire on_delistings if the slice contains delisting events.
                if !slice.delistings.is_empty() {
                    let delistings = slice_proxy.delistings_cell.clone_ref(py);
                    adapter.on_delistings(py, delistings);
                }

                // Fire on_symbol_changed_events if the slice contains rename events.
                if !slice.symbol_changed_events.is_empty() {
                    let sce = slice_proxy.symbol_changed_events_cell.clone_ref(py);
                    adapter.on_symbol_changed_events(py, sce);
                }
            });

            // ── Algorithm Framework pipeline ──────────────────────────────
            // Run alpha → PCM → risk → execution after on_data, outside GIL.
            // Only fires when at least one alpha model has been registered.
            {
                let order_requests =
                    run_framework_pipeline(&adapter.framework, &adapter.inner, &slice);
                if !order_requests.is_empty() {
                    let mut alg = adapter.inner.lock().unwrap();
                    for req in order_requests {
                        submit_execution_order_request(&mut alg, req);
                    }
                }
            }

            let mut update_events =
                drain_local_update_requests(&order_processor, utc_time, "backtest");
            OrderEventProcessingContext {
                adapter: &mut adapter,
                portfolio: &portfolio,
                order_processor: &order_processor,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
                live_writer: as_sidecar_writer(live_writer.as_ref()),
            }
            .process(&mut update_events);

            let mut cancel_events =
                drain_local_cancel_requests(&order_processor, utc_time, "backtest");
            OrderEventProcessingContext {
                adapter: &mut adapter,
                portfolio: &portfolio,
                order_processor: &order_processor,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
                live_writer: as_sidecar_writer(live_writer.as_ref()),
            }
            .process(&mut cancel_events);

            // Process orders submitted from on_data/framework against the same
            // slice. The intraday path already does this after on_data; daily
            // mode needs the same pass so market orders do not wait for the next
            // daily bar when current data is already available.
            let pending_option_order_sids: HashSet<u64> = order_processor
                .transaction_manager
                .get_open_orders()
                .into_iter()
                .filter(|order| order.symbol.option_symbol_id().is_some())
                .map(|order| order.symbol.id.sid)
                .collect();
            if !pending_option_order_sids.is_empty() {
                for (_, (underlying_sym, bars)) in &option_eod_bars_for_day {
                    for bar in bars {
                        let Some(symbol) = option_eod_bar_symbol(bar, underlying_sym) else {
                            continue;
                        };
                        if pending_option_order_sids.contains(&symbol.id.sid) {
                            if let Some(order_bar) =
                                option_eod_bar_to_order_trade_bar(bar, symbol, utc_time)
                            {
                                bars_map.insert(order_bar.symbol.id.sid, order_bar);
                            }
                        }
                    }
                }
            }

            let mut fill_events = order_processor.generate_order_events(&bars_map, utc_time);
            OrderEventProcessingContext {
                adapter: &mut adapter,
                portfolio: &portfolio,
                order_processor: &order_processor,
                all_order_events: &mut all_order_events,
                trade_builder: &mut trade_builder,
                completed_trades: &mut completed_trades,
                live_writer: as_sidecar_writer(live_writer.as_ref()),
            }
            .process(&mut fill_events);

            adapter.on_end_of_day(None);

            process_option_expirations(&mut adapter, current_date, &split_ratios_today);

            let day_equity = portfolio.total_portfolio_value();
            equity_curve.push(day_equity);
            daily_dates.push(current_date.to_string());
            if let Some(writer) = &mut live_writer {
                writer.record_progress(
                    current_date,
                    trading_days,
                    day_equity,
                    all_order_events.len(),
                    completed_trades.len(),
                );
            }

            current_date += chrono::Duration::days(1);
        }
    }

    adapter.on_end_of_algorithm();

    let sim_elapsed = sim_start.elapsed();
    let pts_per_sec = if sim_elapsed.as_secs_f64() > 0.0 {
        trading_days as f64 / sim_elapsed.as_secs_f64()
    } else {
        f64::INFINITY
    };
    println!(
        "Simulation: {:.0} trading days in {:.0}ms ({:.0} days/sec)",
        trading_days,
        sim_elapsed.as_millis(),
        pts_per_sec
    );

    let final_value = {
        use rust_decimal::prelude::ToPrimitive;
        portfolio.total_portfolio_value().to_f64().unwrap_or(0.0)
    };
    let total_return = if starting_cash > 0.0 {
        (final_value - starting_cash) / starting_cash
    } else {
        0.0
    };

    let starting_cash_dec = Decimal::from_f64(starting_cash).unwrap_or(Decimal::ONE);
    let risk_free_rate = Decimal::from_f64(0.05 / 252.0).unwrap_or(Decimal::ZERO);
    let mut statistics = PortfolioStatistics::compute(
        &equity_curve,
        &[],
        &completed_trades,
        trading_days,
        starting_cash_dec,
        risk_free_rate, // ~5% annual risk-free
    );
    let (benchmark_equity_curve, benchmark_aligned_curve) = align_benchmark_curve_to_equity_dates(
        &equity_curve,
        &daily_dates,
        &benchmark_curve,
        &benchmark_dates,
    );
    apply_aligned_benchmark_statistics(
        &mut statistics,
        &benchmark_equity_curve,
        &benchmark_aligned_curve,
        risk_free_rate,
    );

    let equity_curve_f64: Vec<f64> = equity_curve
        .iter()
        .map(|v| {
            use rust_decimal::prelude::ToPrimitive;
            v.to_f64().unwrap_or(0.0)
        })
        .collect();
    let benchmark_curve_f64: Vec<f64> = benchmark_curve
        .iter()
        .map(|v| {
            use rust_decimal::prelude::ToPrimitive;
            v.to_f64().unwrap_or(0.0)
        })
        .collect();
    if let Some(writer) = &live_writer {
        writer.mark_completed(
            trading_days,
            portfolio.total_portfolio_value(),
            all_order_events.len(),
            completed_trades.len(),
        );
    }

    // Collect charts from the strategy after the backtest completes.
    let charts = adapter.charts.lock().map(|c| c.clone()).unwrap_or_default();

    Ok(BacktestResult {
        trading_days,
        final_value,
        total_return,
        starting_cash,
        start_date,
        end_date,
        equity_curve: equity_curve_f64,
        daily_dates,
        benchmark_curve: benchmark_curve_f64,
        benchmark_dates,
        statistics,
        charts,
        order_events: all_order_events,
        succeeded_data_requests,
        failed_data_requests,
        backtest_id,
        benchmark_symbol: effective_benchmark_ticker,
    })
}

/// Before the backtest loop, fetch missing subscription data.
///
/// Daily/hourly data is stored in date-partitioned cache files. Higher
/// resolutions are resolved lazily in the intraday loop, matching LEAN's
/// source-per-date subscription reader behavior.
async fn pre_fetch_all(
    provider: Arc<dyn IHistoricalDataProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<()> {
    if subscriptions.is_empty() {
        return Ok(());
    }

    if let Some(batch_provider) = factor_provider.clone() {
        return pre_fetch_low_resolution_batched(
            batch_provider,
            factor_provider,
            subscriptions,
            start,
            end,
            resolver,
        )
        .await;
    }

    debug!(
        "Checking local data coverage for {} subscriptions with parallelism {} ({} → {})",
        subscriptions.len(),
        SUBSCRIPTION_PREFETCH_CONCURRENCY.min(subscriptions.len()),
        start,
        end
    );

    for chunk in subscriptions.chunks(SUBSCRIPTION_PREFETCH_CONCURRENCY) {
        let mut tasks = tokio::task::JoinSet::new();
        for sub in chunk {
            let provider = provider.clone();
            let factor_provider = factor_provider.clone();
            let sub = sub.clone();
            let resolver = resolver.clone();
            tasks.spawn(async move {
                pre_fetch_subscription(provider, factor_provider, sub, start, end, resolver).await
            });
        }

        while let Some(result) = tasks.join_next().await {
            result.map_err(|e| anyhow::anyhow!("prefetch task failed: {}", e))??;
        }
    }

    Ok(())
}

async fn pre_fetch_low_resolution_batched(
    provider: Arc<dyn SyncHistoryProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<()> {
    #[derive(Clone)]
    struct BatchItem {
        requested_symbol: Symbol,
        provider_symbol: Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
        is_mapped_request: bool,
    }

    if let Some(ref fp) = factor_provider {
        ensure_auxiliary_files_for_subscriptions(fp.clone(), subscriptions, start, end, resolver)
            .await?;
    }

    let reader = ParquetReader::new();
    let mut items = Vec::new();
    for sub in subscriptions {
        if sub.resolution.is_intraday() || sub.tick_type == TickType::Quote {
            continue;
        }

        let is_equity = matches!(sub.symbol.security_type(), SecurityType::Equity);
        let map_rows = if is_equity {
            let ticker = sub.symbol.permtick.to_lowercase();
            let market = sub.symbol.market().as_str().to_lowercase();
            let map_path = resolver
                .data_root
                .join("equity")
                .join(&market)
                .join("map_files")
                .join(format!("{ticker}.parquet"));
            reader.read_map_file(&map_path).unwrap_or_default()
        } else {
            Vec::new()
        };

        let Some((mapped_start, mapped_end)) = mapped_data_date_range(&map_rows, start, end) else {
            debug!(
                "Skipping data prefetch for {}: requested range {} → {} is outside map-file data range",
                sub.symbol.value, start, end
            );
            continue;
        };

        if local_data_covers_range(sub, mapped_start, mapped_end, resolver).await {
            continue;
        }

        let effective_start = match provider.earliest_date() {
            Some(earliest) if mapped_start < earliest => {
                warn!(
                    "Provider earliest date is {}; clipping backtest start from {} for {}",
                    earliest, mapped_start, sub.symbol.value
                );
                earliest
            }
            _ => mapped_start,
        };

        for (range_start, range_end, mapped_ticker) in
            mapped_ticker_ranges(&map_rows, effective_start, mapped_end, &sub.symbol.permtick)
        {
            let provider_symbol = symbol_with_mapped_ticker(&sub.symbol, &mapped_ticker);
            items.push(BatchItem {
                is_mapped_request: provider_symbol.permtick != sub.symbol.permtick,
                requested_symbol: sub.symbol.clone(),
                provider_symbol,
                resolution: sub.resolution,
                start: range_start,
                end: range_end,
            });
        }
    }

    if items.is_empty() {
        return Ok(());
    }

    debug!(
        "Checking local data coverage for {} low-resolution subscriptions via batched history ({} → {})",
        subscriptions.len(),
        start,
        end
    );

    let mut groups: HashMap<(Resolution, NaiveDate, NaiveDate), Vec<BatchItem>> = HashMap::new();
    for item in items {
        groups
            .entry((item.resolution, item.start, item.end))
            .or_default()
            .push(item);
    }

    for ((resolution, range_start, range_end), group) in groups {
        let mut seen_provider_symbols = HashSet::new();
        let symbols: Vec<Symbol> = group
            .iter()
            .filter_map(|item| {
                let key = (
                    item.provider_symbol.id.sid,
                    item.provider_symbol.permtick.clone(),
                );
                if seen_provider_symbols.insert(key) {
                    Some(item.provider_symbol.clone())
                } else {
                    None
                }
            })
            .collect();

        let request = HistoryBatchRequest {
            symbols,
            resolution,
            start: date_to_datetime(range_start, 0, 0, 0),
            end: date_to_datetime(range_end, 23, 59, 59),
            data_type: DataType::TradeBar,
        };
        let batch = provider.get_history_batch(&request).await?;
        let returned: HashSet<(u64, String)> = batch
            .trade_bars
            .iter()
            .map(|bar| (bar.symbol.id.sid, bar.symbol.permtick.clone()))
            .collect();
        debug!(
            "Downloaded {} {} bars for {} symbols and cached to disk ({} → {})",
            batch.trade_bars.len(),
            resolution,
            group.len(),
            range_start,
            range_end
        );

        let retry_symbols: Vec<Symbol> = group
            .iter()
            .filter(|item| {
                item.is_mapped_request
                    && !returned.contains(&(
                        item.provider_symbol.id.sid,
                        item.provider_symbol.permtick.clone(),
                    ))
            })
            .map(|item| item.requested_symbol.clone())
            .collect();

        if retry_symbols.is_empty() {
            continue;
        }

        debug!(
            "Retrying {} mapped {} requests with requested tickers ({} → {})",
            retry_symbols.len(),
            resolution,
            range_start,
            range_end
        );
        let retry_request = HistoryBatchRequest {
            symbols: retry_symbols,
            resolution,
            start: date_to_datetime(range_start, 0, 0, 0),
            end: date_to_datetime(range_end, 23, 59, 59),
            data_type: DataType::TradeBar,
        };
        let retry_batch = provider.get_history_batch(&retry_request).await?;
        debug!(
            "Downloaded {} retry {} bars and cached to disk ({} → {})",
            retry_batch.trade_bars.len(),
            resolution,
            range_start,
            range_end
        );
    }

    Ok(())
}

async fn ensure_auxiliary_files_for_subscriptions(
    provider: Arc<dyn lean_data_providers::IHistoryProvider>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> Result<()> {
    let reader = ParquetReader::new();
    let mut seen = HashSet::new();
    for sub in subscriptions {
        if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
            continue;
        }
        if !seen.insert(sub.symbol.id.sid) {
            continue;
        }

        let ticker = sub.symbol.permtick.to_lowercase();
        let market = sub.symbol.market().as_str().to_lowercase();
        let sec = format!("{}", sub.symbol.security_type()).to_lowercase();
        let map_path = resolver
            .data_root
            .join("equity")
            .join(&market)
            .join("map_files")
            .join(format!("{ticker}.parquet"));
        if !map_path.exists() {
            let request = lean_data_providers::HistoryRequest {
                symbol: sub.symbol.clone(),
                resolution: Resolution::Daily,
                start: date_to_datetime(start, 0, 0, 0),
                end: date_to_datetime(end, 23, 59, 59),
                data_type: DataType::MapFile,
            };
            provider.get_history(&request).await.map_err(|e| {
                anyhow::anyhow!("Map file generation failed for {}: {e}", sub.symbol.value)
            })?;
        }

        let map_rows = reader.read_map_file(&map_path).map_err(|e| {
            anyhow::anyhow!(
                "Map file missing for {} after provider request: {e}",
                sub.symbol.value
            )
        })?;
        let Some((mapped_start, mapped_end)) = mapped_data_date_range(&map_rows, start, end) else {
            continue;
        };

        let factor_path = resolver
            .data_root
            .join(&sec)
            .join(&market)
            .join("factor_files")
            .join(format!("{ticker}.parquet"));
        let factor_valid = factor_path.exists()
            && reader
                .read_factor_file(&factor_path)
                .is_ok_and(|rows| factor_file_covers_range(&rows, mapped_start, mapped_end));
        if factor_valid {
            continue;
        }

        let request = lean_data_providers::HistoryRequest {
            symbol: sub.symbol.clone(),
            resolution: Resolution::Daily,
            start: date_to_datetime(mapped_start, 0, 0, 0),
            end: date_to_datetime(mapped_end, 23, 59, 59),
            data_type: DataType::FactorFile,
        };
        match provider.get_history(&request).await {
            Ok(_) => debug!("Factor file generated for {}", sub.symbol.value),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Factor file generation failed for {}: {e}",
                    sub.symbol.value
                ));
            }
        }

        let rows = reader.read_factor_file(&factor_path).map_err(|e| {
            anyhow::anyhow!(
                "Factor file missing for {} after provider request: {e}",
                sub.symbol.value
            )
        })?;
        if !factor_file_covers_range(&rows, mapped_start, mapped_end) {
            return Err(anyhow::anyhow!(
                "Factor file for {} does not cover {} -> {} after provider request",
                sub.symbol.value,
                mapped_start,
                mapped_end
            ));
        }
    }
    Ok(())
}

async fn pre_fetch_high_resolution_day_batched(
    provider: Arc<dyn SyncHistoryProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    date: NaiveDate,
    factor_end: NaiveDate,
    resolver: &PathResolver,
) -> Result<usize> {
    #[derive(Clone)]
    struct BatchItem {
        requested_symbol: Symbol,
        provider_symbol: Symbol,
        resolution: Resolution,
        data_type: DataType,
        is_mapped_request: bool,
    }

    let mut items = Vec::new();
    for sub in subscriptions {
        if !sub.resolution.is_intraday() || !is_expected_market_date(&sub.symbol, date) {
            continue;
        }
        if matches!(sub.symbol.security_type(), SecurityType::Equity) {
            if let Some(ref fp) = factor_provider {
                ensure_auxiliary_files_for_subscriptions(
                    fp.clone(),
                    std::slice::from_ref(sub),
                    date,
                    factor_end.max(date),
                    resolver,
                )
                .await?;
            }
        }

        let map_rows = if matches!(sub.symbol.security_type(), SecurityType::Equity) {
            let ticker = sub.symbol.permtick.to_lowercase();
            let market = sub.symbol.market().as_str().to_lowercase();
            let map_path = resolver
                .data_root
                .join("equity")
                .join(&market)
                .join("map_files")
                .join(format!("{ticker}.parquet"));

            let reader = ParquetReader::new();
            let rows = reader.read_map_file(&map_path).unwrap_or_default();
            if mapped_data_date_range(&rows, date, date).is_none() {
                continue;
            }
            rows
        } else {
            Vec::new()
        };

        let requested_symbol = sub.symbol.clone();
        let provider_symbol = mapped_symbol_for_provider(sub.symbol.clone(), &map_rows, date);
        let data_type = if sub.resolution == Resolution::Tick {
            DataType::Tick
        } else if sub.tick_type == TickType::Quote {
            DataType::QuoteBar
        } else {
            DataType::TradeBar
        };
        items.push(BatchItem {
            is_mapped_request: provider_symbol.permtick != requested_symbol.permtick,
            requested_symbol,
            provider_symbol,
            resolution: sub.resolution,
            data_type,
        });
    }

    let mut rows_downloaded = 0usize;
    let mut groups: HashMap<(Resolution, DataType), Vec<BatchItem>> = HashMap::new();
    for item in items {
        groups
            .entry((item.resolution, item.data_type))
            .or_default()
            .push(item);
    }

    let mut groups = groups.into_iter().collect::<Vec<_>>();
    groups.sort_by_key(|((resolution, data_type), _)| {
        (
            resolution_prefetch_rank(*resolution),
            data_type_prefetch_rank(*data_type),
        )
    });

    for ((resolution, data_type), group) in groups {
        let mut seen_provider_symbols = HashSet::new();
        let symbols: Vec<Symbol> = group
            .iter()
            .filter_map(|item| {
                let key = (
                    item.provider_symbol.id.sid,
                    item.provider_symbol.permtick.clone(),
                );
                if seen_provider_symbols.insert(key) {
                    Some(item.provider_symbol.clone())
                } else {
                    None
                }
            })
            .collect();
        if symbols.len() < group.len() {
            debug!(
                "Coalesced {} duplicate {} {:?} batch subscriptions for {}",
                group.len() - symbols.len(),
                resolution,
                data_type,
                date
            );
        }
        let request = HistoryBatchRequest {
            symbols,
            resolution,
            start: date_to_datetime(date, 0, 0, 0),
            end: date_to_datetime(date, 23, 59, 59),
            data_type,
        };
        let batch = provider.get_history_batch(&request).await?;
        let returned_sids: HashSet<u64> = match data_type {
            DataType::TradeBar => {
                rows_downloaded += batch.trade_bars.len();
                batch
                    .trade_bars
                    .iter()
                    .map(|bar| bar.symbol.id.sid)
                    .collect()
            }
            DataType::QuoteBar => {
                rows_downloaded += batch.quote_bars.len();
                batch
                    .quote_bars
                    .iter()
                    .map(|bar| bar.symbol.id.sid)
                    .collect()
            }
            DataType::Tick => {
                rows_downloaded += batch.ticks.len();
                batch.ticks.iter().map(|tick| tick.symbol.id.sid).collect()
            }
            DataType::MarginInterestRate => {
                rows_downloaded += batch.margin_interest_rates.len();
                batch
                    .margin_interest_rates
                    .iter()
                    .map(|rate| rate.symbol.id.sid)
                    .collect()
            }
            DataType::PerpetualContext => {
                rows_downloaded += batch.perpetual_contexts.len();
                batch
                    .perpetual_contexts
                    .iter()
                    .map(|context| context.symbol.id.sid)
                    .collect()
            }
            DataType::OpenInterest | DataType::FactorFile | DataType::MapFile => HashSet::new(),
        };

        let mut seen_fallback_symbols = HashSet::new();
        let fallback_symbols: Vec<Symbol> = group
            .iter()
            .filter(|item| {
                item.is_mapped_request && !returned_sids.contains(&item.requested_symbol.id.sid)
            })
            .filter_map(|item| {
                if seen_fallback_symbols.insert(item.requested_symbol.id.sid) {
                    Some(item.requested_symbol.clone())
                } else {
                    None
                }
            })
            .collect();
        if fallback_symbols.is_empty() {
            continue;
        }

        debug!(
            "Retrying {} mapped {} {:?} requests for {} with requested tickers",
            fallback_symbols.len(),
            resolution,
            data_type,
            date
        );
        let retry_request = HistoryBatchRequest {
            symbols: fallback_symbols,
            resolution,
            start: date_to_datetime(date, 0, 0, 0),
            end: date_to_datetime(date, 23, 59, 59),
            data_type,
        };
        let retry_batch = provider.get_history_batch(&retry_request).await?;
        rows_downloaded += match data_type {
            DataType::TradeBar => retry_batch.trade_bars.len(),
            DataType::QuoteBar => retry_batch.quote_bars.len(),
            DataType::Tick => retry_batch.ticks.len(),
            DataType::MarginInterestRate => retry_batch.margin_interest_rates.len(),
            DataType::PerpetualContext => retry_batch.perpetual_contexts.len(),
            DataType::OpenInterest | DataType::FactorFile | DataType::MapFile => 0,
        };
    }

    Ok(rows_downloaded)
}

fn resolution_prefetch_rank(resolution: Resolution) -> u8 {
    match resolution {
        Resolution::Tick => 0,
        Resolution::Second => 1,
        Resolution::Minute => 2,
        Resolution::Hour => 3,
        Resolution::Daily => 4,
    }
}

fn data_type_prefetch_rank(data_type: DataType) -> u8 {
    match data_type {
        DataType::TradeBar => 0,
        DataType::QuoteBar => 1,
        DataType::Tick => 2,
        DataType::MarginInterestRate => 3,
        DataType::PerpetualContext => 4,
        DataType::OpenInterest => 5,
        DataType::FactorFile => 6,
        DataType::MapFile => 7,
    }
}

async fn pre_fetch_subscription(
    provider: Arc<dyn IHistoricalDataProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    sub: Arc<SubscriptionDataConfig>,
    start: NaiveDate,
    end: NaiveDate,
    resolver: PathResolver,
) -> Result<()> {
    let is_equity = matches!(sub.symbol.security_type(), SecurityType::Equity);
    let ticker = sub.symbol.permtick.to_lowercase();
    let market = sub.symbol.market().as_str().to_lowercase();
    let sec = format!("{}", sub.symbol.security_type()).to_lowercase();
    let factor_path = resolver
        .data_root
        .join(&sec)
        .join(&market)
        .join("factor_files")
        .join(format!("{ticker}.parquet"));
    let map_path = resolver
        .data_root
        .join("equity")
        .join(&market)
        .join("map_files")
        .join(format!("{ticker}.parquet"));

    if is_equity && !map_path.exists() {
        if let Some(ref fp) = factor_provider {
            debug!(
                "Map file missing for {} — requesting from provider",
                sub.symbol.value
            );
            let request = lean_data_providers::HistoryRequest {
                symbol: sub.symbol.clone(),
                resolution: lean_core::Resolution::Daily,
                start: date_to_datetime(start, 0, 0, 0),
                end: date_to_datetime(end, 23, 59, 59),
                data_type: lean_data_providers::DataType::MapFile,
            };
            match fp.get_history(&request).await {
                Ok(_) => debug!("Map file generated for {}", sub.symbol.value),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Map file generation failed for {}: {e}",
                        sub.symbol.value
                    ));
                }
            }
        }
    }

    let map_rows = if is_equity {
        ParquetReader::new()
            .read_map_file(&map_path)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let effective_range = mapped_data_date_range(&map_rows, start, end);
    let Some((mapped_start, mapped_end)) = effective_range else {
        debug!(
            "Skipping data prefetch for {}: requested range {} → {} is outside map-file data range",
            sub.symbol.value, start, end
        );
        return Ok(());
    };

    // A factor file is valid if it exists, is non-empty, the sentinel row
    // covers the backtest end, and the oldest row covers the requested start.
    // Older rlean/Massive builds could write sentinel-only files for a narrow
    // later request; accepting those for a longer backtest skips historical
    // splits such as DPST's 2023-06-05 reverse split.
    let factor_valid = !is_equity
        || factor_path.exists() && {
            let r = ParquetReader::new();
            r.read_factor_file(&factor_path)
                .is_ok_and(|rows| factor_file_covers_range(&rows, mapped_start, mapped_end))
        };

    let data_covers_range =
        local_data_covers_range(&sub, mapped_start, mapped_end, &resolver).await;

    if data_covers_range && factor_valid {
        return Ok(());
    }

    // Clip start to the provider's earliest supported date (e.g. ThetaData
    // STANDARD only has data from 2018-01-01; requesting earlier causes 403).
    let effective_start = match provider.earliest_date() {
        Some(earliest) if mapped_start < earliest => {
            warn!(
                "Provider earliest date is {}; clipping backtest start from {} for {}",
                earliest, mapped_start, sub.symbol.value
            );
            earliest
        }
        _ => mapped_start,
    };

    let start_dt = date_to_datetime(effective_start, 0, 0, 0);
    let end_dt = date_to_datetime(mapped_end, 23, 59, 59);

    if !data_covers_range && (sub.resolution.is_intraday() || sub.tick_type == TickType::Quote) {
        let rows_downloaded = pre_fetch_missing_high_resolution_days(
            provider.clone(),
            factor_provider.clone(),
            &sub,
            effective_start,
            mapped_end,
            &resolver,
            &map_rows,
        )
        .await?;
        if rows_downloaded > 0 {
            debug!(
                "Downloaded {} {} rows for {} and cached to disk",
                rows_downloaded, sub.resolution, sub.symbol.value
            );
        } else {
            warn!(
                "Historical provider returned 0 {} rows for {} ({} → {}); no cache file was written",
                sub.resolution, sub.symbol.value, effective_start, mapped_end
            );
        }
    } else if !data_covers_range {
        debug!(
            "Local data missing or incomplete for {} — fetching date-partitioned range from provider ({} → {})",
            sub.symbol.value, effective_start, mapped_end
        );
        let mut bars = Vec::new();
        for (range_start, range_end, mapped_ticker) in
            mapped_ticker_ranges(&map_rows, effective_start, mapped_end, &sub.symbol.permtick)
        {
            let provider_symbol = symbol_with_mapped_ticker(&sub.symbol, &mapped_ticker);
            bars.extend(
                provider
                    .get_trade_bars(
                        provider_symbol,
                        sub.resolution,
                        date_to_datetime(range_start, 0, 0, 0),
                        date_to_datetime(range_end, 23, 59, 59),
                    )
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "historical provider failed for {}: {}",
                            sub.symbol.value,
                            e
                        )
                    })?,
            );
        }
        debug!(
            "Downloaded {} bars for {} and cached to disk",
            bars.len(),
            sub.symbol.value
        );
    }

    let factor_needs_update = is_equity
        && (!factor_path.exists() || {
            let r = ParquetReader::new();
            r.read_factor_file(&factor_path)
                .map(|rows| !factor_file_covers_range(&rows, mapped_start, mapped_end))
                .unwrap_or(true)
        });
    if factor_needs_update {
        if let Some(ref fp) = factor_provider {
            debug!(
                "Factor file missing or stale for {} — requesting from provider",
                sub.symbol.value
            );
            let fp = Arc::clone(fp);
            let request = lean_data_providers::HistoryRequest {
                symbol: sub.symbol.clone(),
                resolution: lean_core::Resolution::Daily,
                start: start_dt,
                end: end_dt,
                data_type: lean_data_providers::DataType::FactorFile,
            };
            match fp.get_history(&request).await {
                Ok(_) => debug!("Factor file generated for {}", sub.symbol.value),
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Factor file generation failed for {}: {e}",
                        sub.symbol.value
                    ));
                }
            }

            let rows = ParquetReader::new()
                .read_factor_file(&factor_path)
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Factor file missing for {} after provider request: {e}",
                        sub.symbol.value
                    )
                })?;
            if !factor_file_covers_range(&rows, mapped_start, mapped_end) {
                return Err(anyhow::anyhow!(
                    "Factor file for {} does not cover {} -> {} after provider request",
                    sub.symbol.value,
                    mapped_start,
                    mapped_end
                ));
            }
        } else if matches!(sub.symbol.security_type(), SecurityType::Equity) {
            warn!(
                "Factor file missing or stale for {} but no factor provider is configured.",
                sub.symbol.value
            );
        }
    }

    Ok(())
}

async fn pre_fetch_missing_high_resolution_days(
    provider: Arc<dyn IHistoricalDataProvider>,
    factor_provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
    map_rows: &[MapFileEntry],
) -> Result<usize> {
    let dates = expected_market_dates(&sub.symbol, start, end);
    let reader = ParquetReader::new();
    let mut missing = Vec::new();
    for date in dates {
        if !cached_partition_has_symbol_data(&reader, resolver, sub, date) {
            missing.push(date);
        }
    }

    if missing.is_empty() {
        return Ok(0);
    }
    debug!(
        "Local data missing or incomplete for {} — fetching {} missing {} files date-by-date with parallelism {} ({} → {})",
        sub.symbol.value,
        missing.len(),
        sub.resolution,
        HIGH_RESOLUTION_PREFETCH_CONCURRENCY.min(missing.len()),
        missing.first().unwrap(),
        missing.last().unwrap()
    );

    let mut rows_downloaded = 0usize;
    let mut completed = 0usize;
    for chunk in missing.chunks(HIGH_RESOLUTION_PREFETCH_CONCURRENCY) {
        let mut tasks = tokio::task::JoinSet::new();

        for date in chunk.iter().copied() {
            let provider = provider.clone();
            let factor_provider = factor_provider.clone();
            let symbol = sub.symbol.clone();
            let resolution = sub.resolution;
            let tick_type = sub.tick_type;
            let symbol_value = sub.symbol.value.clone();
            let map_rows = map_rows.to_vec();

            tasks.spawn(async move {
                let start_dt = date_to_datetime(date, 0, 0, 0);
                let end_dt = date_to_datetime(date, 23, 59, 59);
                let requested_symbol = symbol.clone();
                let provider_symbol = mapped_symbol_for_provider(symbol, &map_rows, date);
                let is_mapped_request =
                    provider_symbol.permtick != requested_symbol.permtick;
                let mapped_ticker = provider_symbol.permtick.clone();

                if resolution == Resolution::Tick || tick_type == TickType::Quote {
                    let Some(fp) = factor_provider else {
                        warn!(
                            "No sync history provider configured for {} {} download",
                            symbol_value, resolution
                        );
                        return Ok((date, 0usize));
                    };
                    let request = lean_data_providers::HistoryRequest {
                        symbol: provider_symbol,
                        resolution,
                        start: start_dt,
                        end: end_dt,
                        data_type: if resolution == Resolution::Tick {
                            lean_data_providers::DataType::Tick
                        } else {
                            lean_data_providers::DataType::QuoteBar
                        },
                    };
                    let mut rows = if resolution == Resolution::Tick {
                        match fp.get_ticks(&request).await {
                            Ok(rows) => rows.len(),
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "historical provider failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                ));
                            }
                        }
                    } else {
                        match fp.get_quote_bars(&request).await {
                            Ok(rows) => rows.len(),
                            Err(e) => {
                                return Err(anyhow::anyhow!(
                                    "historical provider failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                ));
                            }
                        }
                    };
                    if rows == 0 && is_mapped_request {
                        debug!(
                            "Mapped {} request for {} returned 0 rows on {}; retrying requested ticker {}",
                            mapped_ticker,
                            requested_symbol.permtick,
                            date,
                            requested_symbol.permtick
                        );
                        let retry_request = lean_data_providers::HistoryRequest {
                            symbol: requested_symbol,
                            resolution,
                            start: start_dt,
                            end: end_dt,
                            data_type: request.data_type,
                        };
                        rows = if resolution == Resolution::Tick {
                            match fp.get_ticks(&retry_request).await {
                                Ok(rows) => rows.len(),
                                Err(e) => {
                                    return Err(anyhow::anyhow!(
                                        "historical provider retry failed for {} {}: {}",
                                        symbol_value,
                                        date,
                                        e
                                    ));
                                }
                            }
                        } else {
                            match fp.get_quote_bars(&retry_request).await {
                                Ok(rows) => rows.len(),
                                Err(e) => {
                                    return Err(anyhow::anyhow!(
                                        "historical provider retry failed for {} {}: {}",
                                        symbol_value,
                                        date,
                                        e
                                    ));
                                }
                            }
                        };
                    }
                    Ok((date, rows))
                } else {
                    let mut bars = provider
                        .get_trade_bars(provider_symbol, resolution, start_dt, end_dt)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!(
                                "historical provider failed for {} {}: {}",
                                symbol_value,
                                date,
                                e
                            )
                        })?;
                    if bars.is_empty() && is_mapped_request {
                        debug!(
                            "Mapped {} request for {} returned 0 rows on {}; retrying requested ticker {}",
                            mapped_ticker,
                            requested_symbol.permtick,
                            date,
                            requested_symbol.permtick
                        );
                        bars = provider
                            .get_trade_bars(requested_symbol, resolution, start_dt, end_dt)
                            .await
                            .map_err(|e| {
                                anyhow::anyhow!(
                                    "historical provider retry failed for {} {}: {}",
                                    symbol_value,
                                    date,
                                    e
                                )
                            })?;
                    }
                    Ok((date, bars.len()))
                }
            });
        }

        while let Some(result) = tasks.join_next().await {
            let (date, rows) =
                result.map_err(|e| anyhow::anyhow!("historical provider task failed: {}", e))??;
            completed += 1;
            rows_downloaded += rows;

            if completed == 1 || completed.is_multiple_of(50) || completed == missing.len() {
                debug!(
                    "Fetched {} {} missing day {}/{} ({})",
                    sub.symbol.value,
                    sub.resolution,
                    completed,
                    missing.len(),
                    date
                );
            }
        }
    }

    Ok(rows_downloaded)
}

async fn local_data_covers_range(
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
    resolver: &PathResolver,
) -> bool {
    let expected_dates = expected_market_dates(&sub.symbol, start, end);
    if expected_dates.is_empty() {
        return true;
    }

    if sub.resolution.is_intraday() || sub.tick_type == TickType::Quote {
        let reader = ParquetReader::new();
        for current in &expected_dates {
            if !cached_partition_has_symbol_data(&reader, resolver, sub, *current) {
                return false;
            }
        }
        return true;
    }

    let reader = ParquetReader::new();
    for current in &expected_dates {
        let path = subscription_data_path(resolver, sub, *current);
        if !path.exists() {
            return false;
        }
        if !cached_partition_has_symbol_data(&reader, resolver, sub, *current) {
            return false;
        }
    }
    true
}

fn subscription_data_path(
    resolver: &PathResolver,
    sub: &SubscriptionDataConfig,
    date: NaiveDate,
) -> PathBuf {
    let tick_type = if sub.resolution == Resolution::Tick {
        sub.tick_type
    } else if sub.tick_type == TickType::Quote {
        TickType::Quote
    } else {
        TickType::Trade
    };
    resolver.market_data_partition(&sub.symbol, sub.resolution, tick_type, date)
}

fn cached_partition_has_symbol_data(
    reader: &ParquetReader,
    resolver: &PathResolver,
    sub: &SubscriptionDataConfig,
    date: NaiveDate,
) -> bool {
    let path = subscription_data_path(resolver, sub, date);
    if !path.exists() {
        return false;
    }

    let params = QueryParams::new()
        .with_time_range(
            date_to_datetime(date, 0, 0, 0),
            date_to_datetime(date, 23, 59, 59),
        )
        .with_symbols(vec![sub.symbol.id.sid]);

    if sub.resolution == Resolution::Tick {
        reader
            .read_tick_partition(&path, &sub.symbol, &params)
            .is_ok_and(|rows| !rows.is_empty())
    } else if sub.tick_type == TickType::Quote {
        reader
            .read_quote_bar_partition(&path, &sub.symbol, &params)
            .is_ok_and(|rows| !rows.is_empty())
    } else {
        reader
            .read_trade_bar_partition(&path, &sub.symbol, &params)
            .is_ok_and(|rows| !rows.is_empty())
    }
}

fn cached_partition_has_symbol_sid(
    reader: &ParquetReader,
    resolver: &PathResolver,
    sub: &SubscriptionDataConfig,
    date: NaiveDate,
    partition_sid_cache: &mut HashMap<PathBuf, HashSet<u64>>,
) -> bool {
    let path = subscription_data_path(resolver, sub, date);
    if !path.exists() {
        return false;
    }

    let symbol_sid_column = if sub.resolution == Resolution::Tick {
        1
    } else {
        2
    };
    let sids = partition_sid_cache.entry(path.clone()).or_insert_with(|| {
        reader
            .read_partition_symbol_sids(&path, symbol_sid_column)
            .unwrap_or_default()
    });
    if sids.contains(&sub.symbol.id.sid) {
        return true;
    }
    false
}

fn load_trade_bar_partitions(
    reader: &ParquetReader,
    resolver: &PathResolver,
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
    params: &QueryParams,
) -> Vec<lean_data::TradeBar> {
    let mut bars = Vec::new();
    for date in expected_market_dates(&sub.symbol, start, end) {
        let path =
            resolver.market_data_partition(&sub.symbol, sub.resolution, TickType::Trade, date);
        if path.exists() {
            bars.extend(
                reader
                    .read_trade_bar_partition(&path, &sub.symbol, params)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|bar| bar.symbol.id.sid == sub.symbol.id.sid),
            );
        }
    }
    bars
}

fn resolve_benchmark_symbol(ticker: &str, subscriptions: &[Arc<SubscriptionDataConfig>]) -> Symbol {
    let normalized = ticker.trim().to_uppercase();
    if let Some(sub) = subscriptions
        .iter()
        .find(|sub| symbol_matches_benchmark_ticker(&sub.symbol, &normalized))
    {
        return sub.symbol.clone();
    }

    if normalized.starts_with("XYZ:") {
        return Symbol::create_crypto_future(&normalized, &Market::hyperliquid());
    }

    Symbol::create_equity(&normalized, &Market::usa())
}

fn symbol_matches_benchmark_ticker(symbol: &Symbol, ticker: &str) -> bool {
    symbol.permtick.eq_ignore_ascii_case(ticker) || symbol.value.eq_ignore_ascii_case(ticker)
}

fn benchmark_symbol_in_subscriptions(
    benchmark: &Symbol,
    subscriptions: &[Arc<SubscriptionDataConfig>],
) -> bool {
    subscriptions.iter().any(|sub| {
        sub.symbol.id.sid == benchmark.id.sid
            || symbol_matches_benchmark_ticker(&sub.symbol, &benchmark.value)
            || symbol_matches_benchmark_ticker(&sub.symbol, &benchmark.permtick)
    })
}

async fn load_internal_benchmark_price_map(
    provider: Option<Arc<dyn IHistoricalDataProvider>>,
    reader: &ParquetReader,
    resolver: &PathResolver,
    symbol: &Symbol,
    start: NaiveDate,
    end: NaiveDate,
) -> HashMap<NaiveDate, Decimal> {
    let mut map = load_internal_benchmark_prices_for_resolution(
        provider.clone(),
        reader,
        resolver,
        symbol,
        Resolution::Daily,
        start,
        end,
    )
    .await;

    if map.is_empty() {
        map = load_internal_benchmark_prices_for_resolution(
            provider,
            reader,
            resolver,
            symbol,
            Resolution::Hour,
            start,
            end,
        )
        .await;
    }

    if map.is_empty() {
        warn!(
            "Benchmark data not found for {} ({} → {}) — proceeding without benchmark",
            symbol.value, start, end
        );
    } else {
        info!(
            "Loaded {} benchmark price points for {}",
            map.len(),
            symbol.value
        );
    }

    map
}

async fn load_internal_benchmark_prices_for_resolution(
    provider: Option<Arc<dyn IHistoricalDataProvider>>,
    reader: &ParquetReader,
    resolver: &PathResolver,
    symbol: &Symbol,
    resolution: Resolution,
    start: NaiveDate,
    end: NaiveDate,
) -> HashMap<NaiveDate, Decimal> {
    let mut latest_by_date: HashMap<NaiveDate, (DateTime, Decimal)> = HashMap::new();

    if let Some(provider) = provider {
        let effective_start = match provider.earliest_date() {
            Some(earliest) if start < earliest => earliest,
            _ => start,
        };
        if effective_start <= end {
            match provider
                .get_trade_bars(
                    symbol.clone(),
                    resolution,
                    date_to_datetime(effective_start, 0, 0, 0),
                    date_to_datetime(end, 23, 59, 59),
                )
                .await
            {
                Ok(bars) => {
                    for bar in bars
                        .into_iter()
                        .filter(|bar| bar.symbol.id.sid == symbol.id.sid)
                    {
                        insert_latest_benchmark_close(&mut latest_by_date, bar.time, bar.close);
                    }
                }
                Err(err) => {
                    debug!(
                        "Could not fetch {} benchmark data for {}: {}",
                        resolution, symbol.value, err
                    );
                }
            }
        }
    }

    let params = QueryParams::new()
        .with_time_range(
            date_to_datetime(start, 0, 0, 0),
            date_to_datetime(end, 23, 59, 59),
        )
        .with_symbols(vec![symbol.id.sid]);
    for date in expected_market_dates(symbol, start, end) {
        let path = resolver.market_data_partition(symbol, resolution, TickType::Trade, date);
        if !path.exists() {
            continue;
        }
        match reader.read_trade_bar_partition(&path, symbol, &params) {
            Ok(bars) => {
                for bar in bars
                    .into_iter()
                    .filter(|bar| bar.symbol.id.sid == symbol.id.sid)
                {
                    insert_latest_benchmark_close(&mut latest_by_date, bar.time, bar.close);
                }
            }
            Err(err) => {
                debug!(
                    "Could not read {} benchmark partition {} for {}: {}",
                    resolution,
                    path.display(),
                    symbol.value,
                    err
                );
            }
        }
    }

    latest_by_date
        .into_iter()
        .map(|(date, (_, close))| (date, close))
        .collect()
}

fn insert_latest_benchmark_close(
    latest_by_date: &mut HashMap<NaiveDate, (DateTime, Decimal)>,
    time: DateTime,
    close: Decimal,
) {
    let date = time.date_utc();
    match latest_by_date.get(&date) {
        Some((current_time, _)) if *current_time >= time => {}
        _ => {
            latest_by_date.insert(date, (time, close));
        }
    }
}

fn align_benchmark_curve_to_equity_dates(
    equity_curve: &[Decimal],
    daily_dates: &[String],
    benchmark_curve: &[Decimal],
    benchmark_dates: &[String],
) -> (Vec<Decimal>, Vec<Decimal>) {
    let benchmark_by_date: HashMap<&str, Decimal> = benchmark_dates
        .iter()
        .zip(benchmark_curve.iter())
        .map(|(date, price)| (date.as_str(), *price))
        .collect();

    let mut aligned_equity = Vec::new();
    let mut aligned_benchmark = Vec::new();
    let mut last_benchmark: Option<Decimal> = None;

    for (idx, date) in daily_dates.iter().enumerate() {
        if let Some(price) = benchmark_by_date.get(date.as_str()) {
            if *price > Decimal::ZERO {
                last_benchmark = Some(*price);
            }
        }

        if let (Some(equity), Some(benchmark)) = (equity_curve.get(idx), last_benchmark) {
            aligned_equity.push(*equity);
            aligned_benchmark.push(benchmark);
        }
    }

    (aligned_equity, aligned_benchmark)
}

fn apply_aligned_benchmark_statistics(
    statistics: &mut PortfolioStatistics,
    aligned_equity: &[Decimal],
    aligned_benchmark: &[Decimal],
    risk_free_rate: Decimal,
) {
    if aligned_equity.len() < 3 || aligned_equity.len() != aligned_benchmark.len() {
        return;
    }

    let equity_returns = price_returns(aligned_equity);
    let benchmark_returns = price_returns(aligned_benchmark);
    let beta = Statistics::beta(&equity_returns, &benchmark_returns);
    let aligned_days = i64::try_from(aligned_equity.len()).unwrap_or(0);
    let equity_total_return =
        total_return_from_price_curve(aligned_equity).unwrap_or(Decimal::ZERO);
    let benchmark_total_return =
        total_return_from_price_curve(aligned_benchmark).unwrap_or(Decimal::ZERO);
    let equity_annual = Statistics::annual_performance(equity_total_return, aligned_days);
    let benchmark_annual = Statistics::annual_performance(benchmark_total_return, aligned_days);

    statistics.beta = beta;
    statistics.alpha = if beta.is_zero() {
        Decimal::ZERO
    } else {
        Statistics::alpha(equity_annual, beta, benchmark_annual, risk_free_rate)
    };
    statistics.tracking_error = Statistics::tracking_error(&equity_returns, &benchmark_returns);
    statistics.information_ratio =
        Statistics::information_ratio(&equity_returns, &benchmark_returns);
    statistics.treynor_ratio = if beta.is_zero() {
        Decimal::ZERO
    } else {
        (equity_annual - risk_free_rate) / beta
    };
}

fn price_returns(prices: &[Decimal]) -> Vec<Decimal> {
    prices
        .windows(2)
        .map(|window| {
            if window[0].is_zero() {
                Decimal::ZERO
            } else {
                (window[1] - window[0]) / window[0]
            }
        })
        .collect()
}

fn total_return_from_price_curve(prices: &[Decimal]) -> Option<Decimal> {
    let first = prices.first().copied()?;
    let last = prices.last().copied()?;
    if first.is_zero() {
        None
    } else {
        Some((last - first) / first)
    }
}

async fn cached_trade_partition(
    cache: &mut HashMap<PathBuf, HashMap<u64, Vec<TradeBar>>>,
    reader: &ParquetReader,
    path: PathBuf,
    symbols_by_sid: &HashMap<u64, Symbol>,
    sid: u64,
    params: &QueryParams,
) -> Vec<TradeBar> {
    if !cache.contains_key(&path) {
        let grouped = match reader
            .read_trade_bar_partition_grouped_async(&path, symbols_by_sid, params)
            .await
        {
            Ok(grouped) => grouped,
            Err(err) => {
                warn!("Failed to read trade partition {}: {}", path.display(), err);
                HashMap::new()
            }
        };
        cache.insert(path.clone(), grouped);
    }
    cache
        .get(&path)
        .and_then(|grouped| grouped.get(&sid))
        .cloned()
        .unwrap_or_default()
}

async fn cached_quote_partition(
    cache: &mut HashMap<PathBuf, HashMap<u64, Vec<QuoteBar>>>,
    reader: &ParquetReader,
    path: PathBuf,
    symbols_by_sid: &HashMap<u64, Symbol>,
    sid: u64,
    params: &QueryParams,
) -> Vec<QuoteBar> {
    if !cache.contains_key(&path) {
        let grouped = match reader
            .read_quote_bar_partition_grouped_async(&path, symbols_by_sid, params)
            .await
        {
            Ok(grouped) => grouped,
            Err(err) => {
                warn!("Failed to read quote partition {}: {}", path.display(), err);
                HashMap::new()
            }
        };
        cache.insert(path.clone(), grouped);
    }
    cache
        .get(&path)
        .and_then(|grouped| grouped.get(&sid))
        .cloned()
        .unwrap_or_default()
}

fn cached_tick_partition(
    cache: &mut HashMap<PathBuf, HashMap<u64, Vec<Tick>>>,
    reader: &ParquetReader,
    path: PathBuf,
    template: &Symbol,
    sid: u64,
    params: &QueryParams,
) -> Vec<Tick> {
    cache
        .entry(path.clone())
        .or_insert_with(|| {
            let rows = reader
                .read_tick_partition(&path, template, params)
                .unwrap_or_default();
            let mut grouped: HashMap<u64, Vec<Tick>> = HashMap::new();
            for row in rows {
                grouped.entry(row.symbol.id.sid).or_default().push(row);
            }
            grouped
        })
        .get(&sid)
        .cloned()
        .unwrap_or_default()
}

fn expected_market_dates(symbol: &Symbol, start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut dates = Vec::new();
    let mut current = start;
    while current <= end {
        if is_expected_market_date(symbol, current) {
            dates.push(current);
        }
        current += chrono::Duration::days(1);
    }
    dates
}

fn is_expected_market_date(symbol: &Symbol, date: NaiveDate) -> bool {
    match symbol.security_type() {
        SecurityType::Equity | SecurityType::Option | SecurityType::IndexOption => {
            let hours = ExchangeHours::us_equity();
            let dow = date.weekday().num_days_from_sunday() as usize;
            hours.schedule[dow].is_open() && !hours.holidays.contains(&date)
        }
        SecurityType::Crypto | SecurityType::CryptoFuture => true,
        _ => !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun),
    }
}

fn crypto_future_symbols(subscriptions: &[Arc<SubscriptionDataConfig>]) -> Vec<Symbol> {
    let mut seen = HashSet::new();
    let mut symbols = Vec::new();
    for sub in subscriptions {
        if sub.symbol.security_type() == SecurityType::CryptoFuture
            && seen.insert(sub.symbol.id.sid)
        {
            symbols.push(sub.symbol.clone());
        }
    }
    symbols
}

async fn ensure_crypto_future_margin_interest_rates_for_date(
    provider: Arc<dyn lean_data_providers::IHistoryProvider>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    date: NaiveDate,
    resolver: &PathResolver,
) -> Result<usize> {
    let mut rows = 0usize;
    let reader = ParquetReader::new();
    for symbol in crypto_future_symbols(subscriptions) {
        if margin_interest_partition_has_symbol_data(&reader, resolver, &symbol, date) {
            continue;
        }
        let request = lean_data_providers::HistoryRequest {
            symbol: symbol.clone(),
            resolution: Resolution::Hour,
            start: date_to_datetime(date, 0, 0, 0),
            end: date_to_datetime(date, 23, 59, 59),
            data_type: DataType::MarginInterestRate,
        };
        let fetched = provider.get_margin_interest_rates(&request).await?;
        rows += fetched.len();
    }
    Ok(rows)
}

async fn ensure_crypto_future_perpetual_contexts_for_date(
    provider: Arc<dyn lean_data_providers::IHistoryProvider>,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    date: NaiveDate,
    resolver: &PathResolver,
) -> Result<usize> {
    let mut rows = 0usize;
    let reader = ParquetReader::new();
    let mut missing_symbols = Vec::new();
    for symbol in crypto_future_symbols(subscriptions) {
        if perpetual_context_partition_has_symbol_data(&reader, resolver, &symbol, date) {
            continue;
        }
        missing_symbols.push(symbol);
    }
    if !missing_symbols.is_empty() {
        let request = lean_data_providers::HistoryBatchRequest {
            symbols: missing_symbols,
            resolution: Resolution::Minute,
            start: date_to_datetime(date, 0, 0, 0),
            end: date_to_datetime(date, 23, 59, 59),
            data_type: DataType::PerpetualContext,
        };
        let fetched = provider.get_history_batch(&request).await?;
        rows += fetched.perpetual_contexts.len();
    }
    Ok(rows)
}

fn margin_interest_partition_has_symbol_data(
    reader: &ParquetReader,
    resolver: &PathResolver,
    symbol: &Symbol,
    date: NaiveDate,
) -> bool {
    let path = resolver.margin_interest_partition(symbol, date);
    if !path.exists() {
        return false;
    }
    let params = QueryParams::new()
        .with_time_range(
            date_to_datetime(date, 0, 0, 0),
            date_to_datetime(date, 23, 59, 59),
        )
        .with_symbols(vec![symbol.id.sid]);
    reader
        .read_margin_interest_rate_partition(&path, symbol, &params)
        .is_ok_and(|rows| !rows.is_empty())
}

fn perpetual_context_partition_has_symbol_data(
    reader: &ParquetReader,
    resolver: &PathResolver,
    symbol: &Symbol,
    date: NaiveDate,
) -> bool {
    let path = resolver.perpetual_context_partition(symbol, date);
    if !path.exists() {
        return false;
    }
    let params = QueryParams::new()
        .with_time_range(
            date_to_datetime(date, 0, 0, 0),
            date_to_datetime(date, 23, 59, 59),
        )
        .with_symbols(vec![symbol.id.sid]);
    reader
        .read_perpetual_context_partition(&path, symbol, &params)
        .is_ok_and(|rows| !rows.is_empty())
}

fn load_margin_interest_rates_for_date(
    reader: &ParquetReader,
    resolver: &PathResolver,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    date: NaiveDate,
) -> Result<HashMap<u64, Vec<MarginInterestRate>>> {
    let params = QueryParams::new().with_time_range(
        date_to_datetime(date, 0, 0, 0),
        date_to_datetime(date, 23, 59, 59),
    );
    let mut by_sid = HashMap::new();
    for symbol in crypto_future_symbols(subscriptions) {
        let path = resolver.margin_interest_partition(&symbol, date);
        if !path.exists() {
            continue;
        }
        let mut symbol_params = params.clone();
        symbol_params.predicate = symbol_params.predicate.with_symbols(vec![symbol.id.sid]);
        let mut rates =
            reader.read_margin_interest_rate_partition(&path, &symbol, &symbol_params)?;
        rates.retain(|rate| rate.symbol.id.sid == symbol.id.sid);
        if !rates.is_empty() {
            by_sid.insert(symbol.id.sid, rates);
        }
    }
    Ok(by_sid)
}

fn load_perpetual_contexts_for_date(
    reader: &ParquetReader,
    resolver: &PathResolver,
    subscriptions: &[Arc<SubscriptionDataConfig>],
    date: NaiveDate,
) -> Result<HashMap<u64, Vec<PerpetualContext>>> {
    let params = QueryParams::new().with_time_range(
        date_to_datetime(date, 0, 0, 0),
        date_to_datetime(date, 23, 59, 59),
    );
    let mut by_sid = HashMap::new();
    for symbol in crypto_future_symbols(subscriptions) {
        let path = resolver.perpetual_context_partition(&symbol, date);
        if !path.exists() {
            continue;
        }
        let mut symbol_params = params.clone();
        symbol_params.predicate = symbol_params.predicate.with_symbols(vec![symbol.id.sid]);
        let mut contexts =
            reader.read_perpetual_context_partition(&path, &symbol, &symbol_params)?;
        contexts.retain(|context| context.symbol.id.sid == symbol.id.sid);
        if !contexts.is_empty() {
            by_sid.insert(symbol.id.sid, contexts);
        }
    }
    Ok(by_sid)
}

// ─── helpers ─────────────────────────────────────────────────────────────────

pub(crate) fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap()))
}

fn day_key(date: NaiveDate) -> i64 {
    date.signed_duration_since(NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
        .num_days()
}

struct OptionChainRuntime {
    permtick: String,
    chain: OptionChain,
    trade_updates: HashMap<i64, Vec<TradeBar>>,
    quote_updates: HashMap<i64, Vec<QuoteBar>>,
    tick_updates: HashMap<i64, Vec<Tick>>,
    tick_stream: Option<TickStream>,
    pending_tick: Option<Tick>,
    priced_contracts: HashSet<Symbol>,
}

impl OptionChainRuntime {
    fn ticks_at(&self, valuation_time: DateTime) -> Option<&Vec<Tick>> {
        self.tick_updates.get(&valuation_time.0)
    }

    fn prime_tick_stream(&mut self) {
        if self.pending_tick.is_some() {
            return;
        }
        self.pending_tick = self.next_stream_tick();
    }

    fn next_tick_time(&mut self) -> Option<i64> {
        self.prime_tick_stream();
        self.pending_tick.as_ref().map(|tick| tick.time.0)
    }

    fn take_stream_ticks_at(&mut self, timestamp: i64) -> Vec<Tick> {
        self.prime_tick_stream();
        let mut out = Vec::new();
        while self
            .pending_tick
            .as_ref()
            .is_some_and(|tick| tick.time.0 == timestamp)
        {
            if let Some(tick) = self.pending_tick.take() {
                out.push(tick);
            }
            self.pending_tick = self.next_stream_tick();
        }
        out
    }

    fn next_stream_tick(&mut self) -> Option<Tick> {
        let stream = self.tick_stream.as_mut()?;
        loop {
            match stream.next() {
                Some(Ok(tick)) => return Some(tick),
                Some(Err(e)) => {
                    warn!("option tick stream row failed for {}: {e}", self.permtick);
                    continue;
                }
                None => return None,
            }
        }
    }

    fn apply_timestamp(
        &mut self,
        valuation_time: DateTime,
        spot: Decimal,
        stream_ticks: &[Tick],
    ) -> bool {
        if spot <= Decimal::ZERO && self.chain.underlying_price <= Decimal::ZERO {
            return false;
        }

        let mut changed = !stream_ticks.is_empty();
        let effective_spot = if spot > Decimal::ZERO {
            spot
        } else {
            self.chain.underlying_price
        };
        let spot_changed = self.chain.underlying_price != effective_spot;
        self.chain.underlying_price = effective_spot;
        let mut changed_symbols = HashSet::new();

        if let Some(bars) = self.trade_updates.get(&valuation_time.0) {
            changed |= !bars.is_empty();
            for bar in bars {
                if apply_option_trade_bar(&mut self.chain, bar, effective_spot) {
                    changed_symbols.insert(bar.symbol.clone());
                }
            }
        }
        if let Some(bars) = self.quote_updates.get(&valuation_time.0) {
            changed |= !bars.is_empty();
            for bar in bars {
                if apply_option_quote_bar(&mut self.chain, bar, effective_spot) {
                    changed_symbols.insert(bar.symbol.clone());
                }
            }
        }
        if let Some(ticks) = self.tick_updates.get(&valuation_time.0) {
            changed |= !ticks.is_empty();
            for tick in ticks {
                if apply_option_tick(&mut self.chain, tick, effective_spot) {
                    changed_symbols.insert(tick.symbol.clone());
                }
            }
        }
        for tick in stream_ticks {
            if apply_option_tick(&mut self.chain, tick, effective_spot) {
                changed_symbols.insert(tick.symbol.clone());
            }
        }

        if spot_changed {
            changed_symbols.extend(self.priced_contracts.iter().cloned());
            for symbol in &self.priced_contracts {
                if let Some(contract) = self.chain.contracts.get_mut(symbol) {
                    contract.data.underlying_last_price = effective_spot;
                }
            }
        }

        if changed || spot_changed {
            reprice_option_contracts(&mut self.chain, valuation_time, &changed_symbols);
            self.priced_contracts.extend(changed_symbols);
        }
        changed || spot_changed
    }

    fn timestamps(&self) -> Vec<i64> {
        let mut out = std::collections::BTreeSet::new();
        out.extend(self.trade_updates.keys().copied());
        out.extend(self.quote_updates.keys().copied());
        out.extend(self.tick_updates.keys().copied());
        out.into_iter().collect()
    }
}

struct OptionChainRuntimeRequest<'a> {
    data_root: &'a Path,
    ticker: &'a str,
    canonical: &'a Symbol,
    resolution: Resolution,
    date: NaiveDate,
    spot: Decimal,
    filter: Option<OptionFilter>,
    held_contracts: Vec<Symbol>,
    universe_rows: Option<Vec<OptionUniverseRow>>,
    provider: Option<&'a Arc<dyn lean_data_providers::IHistoryProvider>>,
}

#[derive(Clone)]
struct OwnedOptionChainRuntimeRequest {
    data_root: PathBuf,
    ticker: String,
    canonical: Symbol,
    resolution: Resolution,
    date: NaiveDate,
    spot: Decimal,
    filter: Option<OptionFilter>,
    held_contracts: Vec<Symbol>,
    universe_rows: Option<Vec<OptionUniverseRow>>,
    provider: Option<Arc<dyn lean_data_providers::IHistoryProvider>>,
}

async fn load_owned_option_chain_runtime(
    request: OwnedOptionChainRuntimeRequest,
) -> OptionChainRuntime {
    load_option_chain_runtime(OptionChainRuntimeRequest {
        data_root: &request.data_root,
        ticker: &request.ticker,
        canonical: &request.canonical,
        resolution: request.resolution,
        date: request.date,
        spot: request.spot,
        filter: request.filter,
        held_contracts: request.held_contracts,
        universe_rows: request.universe_rows,
        provider: request.provider.as_ref(),
    })
    .await
}

async fn load_option_chain_runtime(request: OptionChainRuntimeRequest<'_>) -> OptionChainRuntime {
    let OptionChainRuntimeRequest {
        data_root,
        ticker,
        canonical,
        resolution,
        date,
        spot,
        filter,
        held_contracts,
        universe_rows,
        provider,
    } = request;

    let mut universe_rows = if let Some(rows) = universe_rows {
        rows
    } else {
        load_option_universe_rows(data_root, ticker, date, provider).await
    };
    apply_option_universe_filter_to_rows(&mut universe_rows, date, spot, filter);
    append_option_universe_rows_for_contracts(&mut universe_rows, &held_contracts, date);
    let chain = build_option_chain_from_universe_rows(canonical, spot, &universe_rows);
    let allowed_contract_sids = chain
        .contracts
        .keys()
        .map(|symbol| symbol.id.sid)
        .collect::<HashSet<_>>();

    let (trade_updates, quote_updates, tick_updates, tick_stream) = match resolution {
        Resolution::Second | Resolution::Minute | Resolution::Hour => (
            group_trade_bars_by_time(
                load_option_trade_bars(
                    data_root,
                    ticker,
                    resolution,
                    date,
                    &universe_rows,
                    provider,
                )
                .await
                .into_iter()
                .filter(|bar| allowed_contract_sids.contains(&bar.symbol.id.sid))
                .collect(),
            ),
            group_quote_bars_by_time(
                load_option_quote_bars(
                    data_root,
                    ticker,
                    resolution,
                    date,
                    &universe_rows,
                    provider,
                )
                .await
                .into_iter()
                .filter(|bar| allowed_contract_sids.contains(&bar.symbol.id.sid))
                .collect(),
            ),
            HashMap::new(),
            None,
        ),
        Resolution::Tick => (
            HashMap::new(),
            HashMap::new(),
            group_ticks_by_time(
                load_option_ticks_for_selected_contracts(
                    data_root,
                    ticker,
                    date,
                    &universe_rows,
                    provider,
                )
                .await,
            ),
            None,
        ),
        _ => (HashMap::new(), HashMap::new(), HashMap::new(), None),
    };

    OptionChainRuntime {
        permtick: canonical.permtick.clone(),
        chain,
        trade_updates,
        quote_updates,
        tick_updates,
        tick_stream,
        pending_tick: None,
        priced_contracts: HashSet::new(),
    }
}

async fn load_option_universe_rows(
    data_root: &Path,
    ticker: &str,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<OptionUniverseRow> {
    let result = if let Some(provider) = provider {
        provider.get_option_universe(ticker, date).await
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_universe(ticker, date)
            .await
    };

    result.unwrap_or_else(|e| {
        warn!("option universe fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

async fn load_option_universe_rows_for_tickers(
    data_root: &Path,
    tickers: &[String],
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> HashMap<String, Vec<OptionUniverseRow>> {
    if tickers.is_empty() {
        return HashMap::new();
    }

    let result = if let Some(provider) = provider {
        provider.get_option_universes(tickers, date).await
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_universes(tickers, date)
            .await
    };

    result.unwrap_or_else(|e| {
        warn!("option universe batch fetch failed for {date}: {e}");
        HashMap::new()
    })
}

async fn load_option_trade_bars(
    data_root: &Path,
    ticker: &str,
    resolution: Resolution,
    date: NaiveDate,
    contracts: &[OptionUniverseRow],
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<TradeBar> {
    let result = if let Some(provider) = provider {
        provider
            .get_option_trade_bars_filtered(ticker, resolution, date, contracts)
            .await
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_trade_bars_filtered(ticker, resolution, date, contracts)
            .await
    };

    result.unwrap_or_else(|e| {
        warn!("option trade-bar fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

async fn load_option_quote_bars(
    data_root: &Path,
    ticker: &str,
    resolution: Resolution,
    date: NaiveDate,
    contracts: &[OptionUniverseRow],
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<QuoteBar> {
    let result = if let Some(provider) = provider {
        provider
            .get_option_quote_bars_filtered(ticker, resolution, date, contracts)
            .await
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_quote_bars_filtered(ticker, resolution, date, contracts)
            .await
    };

    result.unwrap_or_else(|e| {
        warn!("option quote-bar fetch failed for {ticker} {date}: {e}");
        vec![]
    })
}

async fn load_option_ticks_for_selected_contracts(
    data_root: &Path,
    ticker: &str,
    date: NaiveDate,
    contracts: &[OptionUniverseRow],
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<Tick> {
    if contracts.is_empty() {
        return vec![];
    }
    let result = if let Some(provider) = provider {
        provider
            .get_option_ticks_filtered(ticker, date, contracts)
            .await
    } else {
        lean_data_providers::LocalHistoryProvider::new(data_root)
            .get_option_ticks_filtered(ticker, date, contracts)
            .await
    };

    let mut ticks = result.unwrap_or_else(|e| {
        warn!("option ticks failed for {ticker} {date}: {e}");
        vec![]
    });
    if ticks.is_empty() {
        return ticks;
    }

    let allowed = option_universe_symbol_values(contracts);
    ticks.retain(|tick| allowed.contains(tick.symbol.value.as_str()));
    ticks
}

fn option_contracts_for_canonical(
    open_option_symbols: &[Symbol],
    canonical: &Symbol,
) -> Vec<Symbol> {
    let canonical_underlying = canonical
        .underlying
        .as_ref()
        .map(|underlying| {
            underlying
                .permtick
                .trim_start_matches('?')
                .to_ascii_uppercase()
        })
        .unwrap_or_else(|| {
            canonical
                .permtick
                .trim_start_matches('?')
                .to_ascii_uppercase()
        });

    open_option_symbols
        .iter()
        .filter(|symbol| {
            symbol
                .underlying
                .as_ref()
                .map(|underlying| {
                    underlying
                        .permtick
                        .eq_ignore_ascii_case(&canonical_underlying)
                })
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

fn option_right_from_str(right: &str) -> Option<OptionRight> {
    match right.to_ascii_uppercase().as_str() {
        "C" | "CALL" => Some(OptionRight::Call),
        "P" | "PUT" => Some(OptionRight::Put),
        _ => None,
    }
}

fn option_universe_row_symbol(row: &OptionUniverseRow) -> Option<Symbol> {
    let right = option_right_from_str(&row.right)?;
    let underlying = Symbol::create_equity(&row.underlying, &Market::usa());
    Some(Symbol::create_option_osi(
        underlying,
        row.strike,
        row.expiration,
        right,
        OptionStyle::American,
        &Market::usa(),
    ))
}

fn option_universe_row_from_contract_symbol(
    symbol: &Symbol,
    date: NaiveDate,
) -> Option<OptionUniverseRow> {
    let option_id = symbol.option_symbol_id()?;
    let underlying = symbol
        .underlying
        .as_ref()
        .map(|underlying| underlying.permtick.clone())
        .unwrap_or_else(|| option_id.underlying.permtick.clone());
    let right = match option_id.right {
        OptionRight::Call => "C",
        OptionRight::Put => "P",
    }
    .to_string();

    Some(OptionUniverseRow {
        date,
        symbol_value: symbol.value.clone(),
        underlying,
        expiration: option_id.expiry,
        strike: option_id.strike,
        right,
    })
}

fn append_option_universe_rows_for_contracts(
    rows: &mut Vec<OptionUniverseRow>,
    contracts: &[Symbol],
    date: NaiveDate,
) {
    if contracts.is_empty() {
        return;
    }

    let mut existing_sids: HashSet<u64> = rows
        .iter()
        .filter_map(option_universe_row_symbol)
        .map(|symbol| symbol.id.sid)
        .collect();

    for contract in contracts {
        if existing_sids.contains(&contract.id.sid) {
            continue;
        }
        if let Some(row) = option_universe_row_from_contract_symbol(contract, date) {
            existing_sids.insert(contract.id.sid);
            rows.push(row);
        }
    }
}

fn option_universe_symbol_values(contracts: &[OptionUniverseRow]) -> HashSet<String> {
    contracts
        .iter()
        .filter_map(option_universe_row_symbol)
        .map(|symbol| symbol.value)
        .collect()
}

fn apply_option_universe_filter_to_rows(
    rows: &mut Vec<OptionUniverseRow>,
    today: NaiveDate,
    spot: Decimal,
    filter: Option<OptionFilter>,
) {
    let Some(filter) = filter else {
        return;
    };
    rows.retain(|row| {
        let dte = row.expiration.signed_duration_since(today).num_days() as i32;
        dte >= filter.min_expiry_days && dte <= filter.max_expiry_days
    });
    apply_option_strike_rank_filter(rows, spot, filter, |row| row.expiration, |row| row.strike);
}

fn apply_option_filter_to_eod_bars(
    bars: &mut Vec<OptionEodBar>,
    today: NaiveDate,
    spot: Decimal,
    filter: Option<OptionFilter>,
) {
    let Some(filter) = filter else {
        return;
    };
    bars.retain(|bar| {
        let dte = bar.expiration.signed_duration_since(today).num_days() as i32;
        dte >= filter.min_expiry_days && dte <= filter.max_expiry_days
    });
    apply_option_strike_rank_filter(bars, spot, filter, |bar| bar.expiration, |bar| bar.strike);
}

fn apply_option_strike_rank_filter<T, ExpiryFn, StrikeFn>(
    items: &mut Vec<T>,
    spot: Decimal,
    filter: OptionFilter,
    _expiry_of: ExpiryFn,
    strike_of: StrikeFn,
) where
    ExpiryFn: Fn(&T) -> NaiveDate,
    StrikeFn: Fn(&T) -> Decimal,
{
    if spot <= Decimal::ZERO {
        return;
    }

    let mut strikes = Vec::new();
    for item in items.iter() {
        strikes.push(strike_of(item));
    }
    strikes.sort();
    strikes.dedup();
    if strikes.is_empty() {
        return;
    }

    let mut exact_price_found = true;
    let index = match strikes.binary_search(&spot) {
        Ok(index) => index as i32,
        Err(index) => {
            exact_price_found = false;
            if index == strikes.len() {
                items.clear();
                return;
            }
            index as i32
        }
    };

    let mut index_min_price = index + filter.min_strike_rank;
    let mut index_max_price = index + filter.max_strike_rank;
    if !exact_price_found {
        if filter.min_strike_rank < 0 && filter.max_strike_rank > 0 {
            index_max_price -= 1;
        } else if filter.min_strike_rank > 0 {
            index_min_price -= 1;
            index_max_price -= 1;
        }
    }

    if index_min_price < 0 {
        index_min_price = 0;
    } else if index_min_price >= strikes.len() as i32 {
        items.clear();
        return;
    }

    if index_max_price < 0 {
        items.clear();
        return;
    }
    if index_max_price >= strikes.len() as i32 {
        index_max_price = strikes.len() as i32 - 1;
    }

    let min_price = strikes[index_min_price as usize];
    let max_price = strikes[index_max_price as usize];

    items.retain(|item| {
        let strike = strike_of(item);
        strike >= min_price && strike <= max_price
    });
}

fn build_option_chain_from_universe_rows(
    canonical_sym: &Symbol,
    spot: Decimal,
    rows: &[OptionUniverseRow],
) -> OptionChain {
    let mut chain = OptionChain::new(canonical_sym.clone(), spot);
    let underlying_sym = canonical_sym
        .underlying
        .as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| {
            Symbol::create_equity(
                canonical_sym.permtick.trim_start_matches('?'),
                &Market::usa(),
            )
        });

    for row in rows {
        let right = match row.right.to_ascii_uppercase().as_str() {
            "C" | "CALL" => OptionRight::Call,
            "P" | "PUT" => OptionRight::Put,
            _ => continue,
        };
        let sym = Symbol::create_option_osi(
            underlying_sym.clone(),
            row.strike,
            row.expiration,
            right,
            OptionStyle::American,
            &Market::usa(),
        );
        let mut contract = OptionContract::new(sym);
        contract.data.underlying_last_price = spot;
        chain.add_contract(contract);
    }

    chain
}

fn reprice_option_chain(chain: &mut OptionChain, valuation_time: DateTime) {
    let model = BlackScholesPriceModel;
    for contract in chain.contracts.values_mut() {
        evaluate_contract_with_market_iv(&model, contract, valuation_time, 0.0, 0.0);
    }
}

fn reprice_option_contracts(
    chain: &mut OptionChain,
    valuation_time: DateTime,
    symbols: &HashSet<Symbol>,
) {
    if symbols.is_empty() {
        return;
    }
    let model = BlackScholesPriceModel;
    for symbol in symbols {
        if let Some(contract) = chain.contracts.get_mut(symbol) {
            evaluate_contract_with_market_iv(&model, contract, valuation_time, 0.0, 0.0);
        }
    }
}

fn apply_option_trade_bar(chain: &mut OptionChain, bar: &TradeBar, spot: Decimal) -> bool {
    use rust_decimal::prelude::ToPrimitive;

    if let Some(contract) = chain.contracts.get_mut(&bar.symbol) {
        contract.data.underlying_last_price = spot;
        contract.data.last_price = bar.close;
        contract.data.volume = bar.volume.to_i64().unwrap_or(contract.data.volume);
        return true;
    }
    false
}

fn apply_option_quote_bar(chain: &mut OptionChain, bar: &QuoteBar, spot: Decimal) -> bool {
    use rust_decimal_macros::dec;

    if let Some(contract) = chain.contracts.get_mut(&bar.symbol) {
        contract.data.underlying_last_price = spot;
        contract.data.bid_price = bar.bid.as_ref().map(|b| b.close).unwrap_or(Decimal::ZERO);
        contract.data.ask_price = bar.ask.as_ref().map(|a| a.close).unwrap_or(Decimal::ZERO);
        contract.data.bid_size = bar
            .last_bid_size
            .round()
            .to_i64()
            .unwrap_or(contract.data.bid_size);
        contract.data.ask_size = bar
            .last_ask_size
            .round()
            .to_i64()
            .unwrap_or(contract.data.ask_size);
        if contract.data.last_price <= Decimal::ZERO
            && contract.data.bid_price > Decimal::ZERO
            && contract.data.ask_price > Decimal::ZERO
        {
            contract.data.last_price =
                (contract.data.bid_price + contract.data.ask_price) / dec!(2);
        }
        return true;
    }
    false
}

fn apply_option_tick(chain: &mut OptionChain, tick: &Tick, spot: Decimal) -> bool {
    use rust_decimal::prelude::ToPrimitive;

    if let Some(contract) = chain.contracts.get_mut(&tick.symbol) {
        contract.data.underlying_last_price = spot;
        match tick.tick_type {
            TickType::Trade => {
                contract.data.last_price = tick.value;
                contract.data.volume = tick
                    .quantity
                    .round()
                    .to_i64()
                    .unwrap_or(contract.data.volume);
            }
            TickType::Quote => {
                contract.data.bid_price = tick.bid_price;
                contract.data.ask_price = tick.ask_price;
                contract.data.bid_size = tick
                    .bid_size
                    .round()
                    .to_i64()
                    .unwrap_or(contract.data.bid_size);
                contract.data.ask_size = tick
                    .ask_size
                    .round()
                    .to_i64()
                    .unwrap_or(contract.data.ask_size);
                if contract.data.last_price <= Decimal::ZERO && tick.value > Decimal::ZERO {
                    contract.data.last_price = tick.value;
                }
            }
            TickType::OpenInterest => {
                contract.data.open_interest = tick.value;
            }
        }
        return true;
    }
    false
}

fn group_trade_bars_by_time(bars: Vec<TradeBar>) -> HashMap<i64, Vec<TradeBar>> {
    let mut by_time: HashMap<i64, Vec<TradeBar>> = HashMap::new();
    for bar in bars {
        by_time.entry(bar.time.0).or_default().push(bar);
    }
    by_time
}

fn group_ticks_by_time(ticks: Vec<Tick>) -> HashMap<i64, Vec<Tick>> {
    let mut by_time: HashMap<i64, Vec<Tick>> = HashMap::new();
    for tick in ticks {
        by_time.entry(tick.time.0).or_default().push(tick);
    }
    by_time
}

fn group_quote_bars_by_time(bars: Vec<QuoteBar>) -> HashMap<i64, Vec<QuoteBar>> {
    let mut by_time: HashMap<i64, Vec<QuoteBar>> = HashMap::new();
    for bar in bars {
        by_time.entry(bar.time.0).or_default().push(bar);
    }
    by_time
}

fn quote_bar_mid_ohlc(bar: &QuoteBar) -> Option<(Decimal, Decimal, Decimal, Decimal)> {
    let open = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.open + ask.open) / Decimal::from(2),
        (Some(bid), None) => bid.open,
        (None, Some(ask)) => ask.open,
        (None, None) => return None,
    };
    let high = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.high + ask.high) / Decimal::from(2),
        (Some(bid), None) => bid.high,
        (None, Some(ask)) => ask.high,
        (None, None) => return None,
    };
    let low = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.low + ask.low) / Decimal::from(2),
        (Some(bid), None) => bid.low,
        (None, Some(ask)) => ask.low,
        (None, None) => return None,
    };
    let close = match (&bar.bid, &bar.ask) {
        (Some(bid), Some(ask)) => (bid.close + ask.close) / Decimal::from(2),
        (Some(bid), None) => bid.close,
        (None, Some(ask)) => ask.close,
        (None, None) => return None,
    };
    Some((open, high, low, close))
}

fn synthesize_trade_bar_from_quote_bar(bar: &QuoteBar) -> Option<TradeBar> {
    let (open, high, low, close) = quote_bar_mid_ohlc(bar)?;
    Some(TradeBar::new(
        bar.symbol.clone(),
        bar.time,
        bar.period,
        TradeBarData::new(open, high, low, close, Decimal::ZERO),
    ))
}

fn apply_quote_bar_to_minute<F>(
    sid: u64,
    raw_qbar: QuoteBar,
    current_date: NaiveDate,
    option_underlying_sids: &HashSet<u64>,
    factor_map: &HashMap<u64, Vec<FactorFileEntry>>,
    bars_for_orders: &mut HashMap<u64, TradeBar>,
    minute_quote_bars: &mut HashMap<u64, QuoteBar>,
    minute_slice: &mut Slice,
    mut update_quote_price: F,
) where
    F: FnMut(&Symbol, Decimal, Decimal, Decimal, bool),
{
    let qbar = if !option_underlying_sids.contains(&sid) {
        if let Some(rows) = factor_map.get(&sid) {
            apply_factor_quote_bar(raw_qbar, rows, current_date)
        } else {
            raw_qbar
        }
    } else {
        raw_qbar
    };

    let mid = qbar.mid_close();
    let bid = qbar
        .bid
        .as_ref()
        .map(|bar| bar.close)
        .unwrap_or(Decimal::ZERO);
    let ask = qbar
        .ask
        .as_ref()
        .map(|bar| bar.close)
        .unwrap_or(Decimal::ZERO);
    let update_mid = mid > Decimal::ZERO && !bars_for_orders.contains_key(&sid);
    if update_mid || bid > Decimal::ZERO || ask > Decimal::ZERO {
        update_quote_price(&qbar.symbol, bid, ask, mid, update_mid);
    }
    if let std::collections::hash_map::Entry::Vacant(e) = bars_for_orders.entry(sid) {
        if let Some(synth) = synthesize_trade_bar_from_quote_bar(&qbar) {
            e.insert(synth);
        }
    }
    minute_quote_bars.insert(sid, qbar.clone());
    minute_slice.add_quote_bar(qbar);
}

fn synthesize_trade_bar_from_option_contract(
    contract: &OptionContract,
    time: DateTime,
) -> Option<TradeBar> {
    let price = contract.mid_price();
    if price <= Decimal::ZERO {
        return None;
    }
    Some(TradeBar::new(
        contract.symbol.clone(),
        time,
        TimeSpan::ZERO,
        TradeBarData::new(price, price, price, price, Decimal::ZERO),
    ))
}

fn synthesize_trade_bar_from_ticks(
    symbol: &Symbol,
    time: DateTime,
    ticks: &[Tick],
) -> Option<TradeBar> {
    if ticks.is_empty() {
        return None;
    }

    let trade_prices: Vec<Decimal> = ticks
        .iter()
        .filter(|tick| tick.tick_type == TickType::Trade && tick.value > Decimal::ZERO)
        .map(|tick| tick.value)
        .collect();

    let volume = ticks
        .iter()
        .filter(|tick| tick.tick_type == TickType::Trade)
        .fold(Decimal::ZERO, |acc, tick| acc + tick.quantity);

    let prices = if !trade_prices.is_empty() {
        trade_prices
    } else {
        ticks
            .iter()
            .filter_map(|tick| match tick.tick_type {
                TickType::Trade if tick.value > Decimal::ZERO => Some(tick.value),
                TickType::Quote if tick.value > Decimal::ZERO => Some(tick.value),
                _ => None,
            })
            .collect()
    };

    let open = *prices.first()?;
    let close = *prices.last()?;
    let high = prices.iter().copied().max()?;
    let low = prices.iter().copied().min()?;

    Some(TradeBar::new(
        symbol.clone(),
        time,
        TimeSpan::ZERO,
        TradeBarData::new(open, high, low, close, volume),
    ))
}

/// Process option expirations for `current_date`.
///
/// Scans all open option positions for contracts expiring today, computes
/// intrinsic value, and handles exercise (long) or assignment (short).
///
/// `split_ratios` — map of underlying SID → forward split ratio for splits that
/// occurred on `current_date`.  When an option underlying had a split today, the
/// option's strike is divided by the ratio before the ITM/OTM comparison so that
/// pre-split strikes are evaluated against the post-split spot price correctly.
fn process_option_expirations(
    adapter: &mut PyAlgorithmAdapter,
    current_date: NaiveDate,
    split_ratios: &HashMap<u64, f64>,
) {
    // Collect expiring positions — we need to drop the lock before calling market_order.
    let expiring: Vec<lean_algorithm::qc_algorithm::OpenOptionPosition> = adapter
        .inner
        .lock()
        .unwrap()
        .get_option_positions()
        .into_iter()
        .filter(|pos| pos.expiry == current_date)
        .collect();

    if expiring.is_empty() {
        return;
    }

    for pos in expiring {
        // Get the spot price for the underlying.
        let spot = {
            let alg = adapter.inner.lock().unwrap();
            // Try to find the underlying security by permtick.
            let underlying_ticker = pos
                .symbol
                .underlying
                .as_ref()
                .map(|u| u.permtick.clone())
                .unwrap_or_default();
            let found: Option<Decimal> = alg
                .securities
                .all()
                .find(|s| s.symbol.permtick.eq_ignore_ascii_case(&underlying_ticker))
                .map(|s| s.current_price());
            found.unwrap_or(pos.strike) // Conservative fallback: use strike
        };

        // If the option's underlying had a forward split today, options written in
        // the pre-split era carry a strike in pre-split price terms while `spot` is
        // in post-split terms.  Divide the strike by the split ratio so the
        // comparison is in a consistent price space.
        let effective_strike = if let Some(underlying) = &pos.symbol.underlying {
            if let Some(&ratio) = split_ratios.get(&underlying.id.sid) {
                let ratio_dec = Decimal::from_f64(ratio).unwrap_or(Decimal::ONE);
                if ratio_dec > Decimal::ZERO {
                    let adj = pos.strike / ratio_dec;
                    info!(
                        "Split-adjusted strike for {}: {:.4} → {:.4} (÷{:.4})",
                        pos.symbol.value, pos.strike, adj, ratio
                    );
                    adj
                } else {
                    pos.strike
                }
            } else {
                pos.strike
            }
        } else {
            pos.strike
        };

        let intrinsic = intrinsic_value(spot, effective_strike, pos.right);
        let exercised = intrinsic >= rust_decimal_macros::dec!(0.01);

        if exercised && pos.quantity > Decimal::ZERO {
            // Long position: auto-exercise
            let contracts = pos.quantity;
            let underlying_sym = pos
                .symbol
                .underlying
                .as_ref()
                .map(|u| *u.clone())
                .unwrap_or_else(|| pos.symbol.clone());

            // Shares from exercise: get_exercise_quantity uses LEAN sign convention.
            // For a long call: caller buys 100*qty shares, pays strike*100*qty.
            // For a long put: caller sells 100*qty shares, receives strike*100*qty.
            let exercise_shares = get_exercise_quantity(contracts, pos.right, 100);
            {
                let alg = adapter.inner.lock().unwrap();
                alg.portfolio.settle_fill_without_cash(
                    &pos.symbol,
                    Decimal::ZERO,
                    -contracts,
                    Decimal::from(pos.contract_unit_of_trade),
                );
                alg.portfolio.apply_exercise_with_market_price(
                    &underlying_sym,
                    effective_strike,
                    exercise_shares,
                    spot,
                );
            }

            info!(
                "Option exercised: {} x{} K={} (effective={}) expiry={}",
                pos.symbol.value, contracts, pos.strike, effective_strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, contracts, true);
        } else if exercised && pos.quantity < Decimal::ZERO {
            // Short position: assignment
            let contracts = pos.quantity.abs();
            let underlying_sym = pos
                .symbol
                .underlying
                .as_ref()
                .map(|u| *u.clone())
                .unwrap_or_else(|| pos.symbol.clone());

            {
                let alg = adapter.inner.lock().unwrap();
                alg.portfolio.settle_fill_without_cash(
                    &pos.symbol,
                    Decimal::ZERO,
                    contracts,
                    Decimal::from(pos.contract_unit_of_trade),
                );
                let shares = Decimal::from(100) * contracts;
                let exercise_qty = match pos.right {
                    OptionRight::Put => shares,   // buy stock
                    OptionRight::Call => -shares, // sell (or short) stock
                };
                alg.portfolio.apply_exercise_with_market_price(
                    &underlying_sym,
                    effective_strike,
                    exercise_qty,
                    spot,
                );
            }

            info!(
                "Option assigned: {} x{} K={} (effective={}) expiry={}",
                pos.symbol.value, pos.quantity, pos.strike, effective_strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            adapter.on_assignment_order_event(contract, contracts, true);
        } else {
            // Expired worthless — premium already booked at trade open.
            let entry_price = pos.entry_price;
            adapter
                .inner
                .lock()
                .unwrap()
                .portfolio
                .settle_fill_without_cash(
                    &pos.symbol,
                    Decimal::ZERO,
                    -pos.quantity,
                    Decimal::from(pos.contract_unit_of_trade),
                );
            info!(
                "Option expired worthless: {} x{} K={} expiry={}",
                pos.symbol.value, pos.quantity, pos.strike, pos.expiry
            );
            let contract = OptionContract::new(pos.symbol.clone());
            // LEAN fires on_order_event (not on_assignment_order_event) for OTM expiry.
            adapter.on_otm_expiry(contract, pos.quantity.abs(), spot, entry_price);
        }
    }
}

fn sync_option_holdings_to_chain_prices(
    adapter: &PyAlgorithmAdapter,
    portfolio: &Arc<lean_algorithm::portfolio::SecurityPortfolioManager>,
    chains: &[(&str, &OptionChain)],
) {
    let holdings = portfolio.all_holdings();
    if holdings.is_empty() {
        return;
    }

    let chain_map: HashMap<&str, &OptionChain> = chains.iter().copied().collect();

    for holding in holdings {
        if !holding.is_invested() || holding.symbol.option_symbol_id().is_none() {
            continue;
        }
        let Some(underlying) = holding.symbol.underlying.as_ref() else {
            continue;
        };
        let canonical = format!("?{}", underlying.permtick);
        let Some(chain) = chain_map.get(canonical.as_str()) else {
            continue;
        };
        let Some((_, contract)) = chain
            .contracts
            .iter()
            .find(|(symbol, _)| symbol.id.sid == holding.symbol.id.sid)
        else {
            continue;
        };

        let price = contract.mid_price();
        if price <= Decimal::ZERO {
            continue;
        }

        portfolio.update_prices(&holding.symbol, price);
        adapter
            .inner
            .lock()
            .unwrap()
            .securities
            .update_price(&holding.symbol, price);
    }
}

fn update_option_chain_map_in_place(
    target: &mut HashMap<String, OptionChain>,
    chains: &[(&str, &OptionChain)],
) {
    let active_keys: HashSet<&str> = chains.iter().map(|(permtick, _)| *permtick).collect();
    target.retain(|permtick, _| active_keys.contains(permtick.as_str()));
    for (permtick, chain) in chains {
        if let Some(existing) = target.get_mut(*permtick) {
            existing.update_from(chain);
        } else {
            target.insert((*permtick).to_string(), (*chain).clone());
        }
    }
}

fn option_eod_bar_symbol(bar: &OptionEodBar, underlying_sym: &Symbol) -> Option<Symbol> {
    let right = option_right_from_str(&bar.right)?;
    Some(Symbol::create_option_osi(
        underlying_sym.clone(),
        bar.strike,
        bar.expiration,
        right,
        OptionStyle::American,
        &Market::usa(),
    ))
}

fn option_eod_bar_to_trade_bar(
    bar: &OptionEodBar,
    symbol: Symbol,
    valuation_time: DateTime,
) -> TradeBar {
    let mid = if bar.bid > Decimal::ZERO && bar.ask > Decimal::ZERO {
        (bar.bid + bar.ask) / rust_decimal_macros::dec!(2)
    } else {
        Decimal::ZERO
    };
    let fallback = if bar.close > Decimal::ZERO {
        bar.close
    } else {
        mid
    };
    let open = if bar.open > Decimal::ZERO {
        bar.open
    } else {
        fallback
    };
    let high = if bar.high > Decimal::ZERO {
        bar.high
    } else {
        open
    };
    let low = if bar.low > Decimal::ZERO {
        bar.low
    } else {
        open
    };
    let close = if bar.close > Decimal::ZERO {
        bar.close
    } else {
        open
    };
    let volume = Decimal::from_i64(bar.volume).unwrap_or(Decimal::ZERO);

    TradeBar::new(
        symbol,
        valuation_time,
        TimeSpan::ONE_DAY,
        TradeBarData::new(open, high, low, close, volume),
    )
}

fn option_eod_bar_to_order_trade_bar(
    bar: &OptionEodBar,
    symbol: Symbol,
    valuation_time: DateTime,
) -> Option<TradeBar> {
    let trade_bar = option_eod_bar_to_trade_bar(bar, symbol, valuation_time);
    if trade_bar.open <= Decimal::ZERO {
        return None;
    }
    Some(trade_bar)
}

/// Build a real option chain from ThetaData EOD rows for a single trading day.
///
/// Build an option chain directly from typed `OptionEodBar` rows.
///
/// Avoids the intermediate `V3OptionEod` representation — no string→date or
/// f64→Decimal round-trips.  Contracts expiring on or before `today` are skipped.
fn build_option_chain_from_eod_bars(
    canonical_sym: &Symbol,
    spot: Decimal,
    valuation_time: DateTime,
    bars: &[OptionEodBar],
    filter: Option<OptionFilter>,
    held_contracts: &[Symbol],
) -> OptionChain {
    let today = valuation_time.date_utc();
    let underlying_sym: Symbol = canonical_sym
        .underlying
        .as_ref()
        .map(|u| *u.clone())
        .unwrap_or_else(|| canonical_sym.clone());
    let market = Market::usa();
    let unfiltered_bars = bars;
    let mut bars = bars.to_vec();
    apply_option_filter_to_eod_bars(&mut bars, today, spot, filter);

    if !held_contracts.is_empty() {
        let held_sids: HashSet<u64> = held_contracts
            .iter()
            .map(|contract| contract.id.sid)
            .collect();
        let mut retained_sids: HashSet<u64> = bars
            .iter()
            .filter_map(|bar| option_eod_bar_symbol(bar, &underlying_sym))
            .map(|symbol| symbol.id.sid)
            .collect();
        for bar in unfiltered_bars {
            let Some(symbol) = option_eod_bar_symbol(bar, &underlying_sym) else {
                continue;
            };
            let sid = symbol.id.sid;
            if held_sids.contains(&sid) && retained_sids.insert(sid) {
                bars.push(bar.clone());
            }
        }
    }

    let mut chain = OptionChain::new(canonical_sym.clone(), spot);

    for bar in &bars {
        if bar.expiration < today {
            continue;
        }
        if bar.strike < Decimal::ONE {
            continue;
        }

        let Some(right) = option_right_from_str(&bar.right) else {
            continue;
        };

        let sym = Symbol::create_option_osi(
            underlying_sym.clone(),
            bar.strike,
            bar.expiration,
            right,
            OptionStyle::American,
            &market,
        );

        let mid = if bar.bid > Decimal::ZERO && bar.ask > Decimal::ZERO {
            (bar.bid + bar.ask) / rust_decimal_macros::dec!(2)
        } else {
            bar.close
        };
        let last = if bar.close > Decimal::ZERO {
            bar.close
        } else {
            mid
        };

        let mut contract = OptionContract::new(sym);
        contract.data = OptionContractData {
            underlying_last_price: spot,
            bid_price: bar.bid,
            ask_price: bar.ask,
            last_price: last,
            volume: bar.volume,
            bid_size: bar.bid_size,
            ask_size: bar.ask_size,
            ..Default::default()
        };
        chain.add_contract(contract);
    }

    reprice_option_chain(&mut chain, valuation_time);
    chain
}

/// Check the local partitioned Parquet cache for option EOD bars; download and cache on miss.
async fn load_option_eod_bars(
    data_root: &Path,
    ticker: &str,
    date: NaiveDate,
    provider: Option<&Arc<dyn lean_data_providers::IHistoryProvider>>,
) -> Vec<OptionEodBar> {
    let cache_path =
        PathResolver::new(data_root).option_partition(Resolution::Daily, TickType::Trade, date);

    if cache_path.exists() {
        let reader = ParquetReader::new();
        let cached = reader
            .read_option_eod_bars(std::slice::from_ref(&cache_path))
            .unwrap_or_default()
            .into_iter()
            .filter(|bar| bar.underlying.eq_ignore_ascii_case(ticker))
            .collect::<Vec<_>>();

        if !cached.is_empty() {
            return cached;
        }
    }

    let Some(provider) = provider else {
        return vec![];
    };
    let bars = match provider.get_option_eod_bars(ticker, date).await {
        Ok(b) => b,
        Err(e) => {
            warn!("option EOD fetch failed for {ticker} {date}: {e}");
            return vec![];
        }
    };

    if bars.is_empty() {
        return vec![];
    }

    let mut merged = if cache_path.exists() {
        ParquetReader::new()
            .read_option_eod_bars(std::slice::from_ref(&cache_path))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    merged.retain(|bar| !bar.underlying.eq_ignore_ascii_case(ticker));
    merged.extend_from_slice(&bars);

    // parquet-rs 53.x can panic reading set/map thrift metadata emitted by
    // optional statistics; match the safer cache writer settings used nearby.
    let writer = ParquetWriter::new(WriterConfig {
        bloom_filter: false,
        write_statistics: false,
        ..WriterConfig::default()
    });
    if let Err(e) = writer.write_option_eod_bars(&merged, &cache_path) {
        warn!("failed to cache option EOD bars for {ticker} {date}: {e}");
    }

    bars
}

/// Load custom data points for one subscription/date.
///
/// Parquet-native providers are read directly from provider parquet paths.
/// Text providers use `get_source()`/`reader()` and persist parsed points as
/// framework parquet under `{data_root}/custom/...`.
async fn load_low_resolution_custom_data_for_day(
    custom_subs: &[CustomDataSubscription],
    custom_history: &HashMap<String, HashMap<NaiveDate, Vec<CustomDataPoint>>>,
    config: &RunConfig,
    current_date: NaiveDate,
) -> Result<HashMap<String, Vec<CustomDataPoint>>> {
    let mut custom_data_for_day: HashMap<String, Vec<CustomDataPoint>> = HashMap::new();
    for sub in custom_subs {
        if sub.is_universe() || sub.config.resolution.is_intraday() {
            continue;
        }
        let key = sub.ticker.to_uppercase();
        if let Some(by_date) = custom_history.get(&key) {
            if let Some(pts) = by_date.get(&current_date) {
                custom_data_for_day.insert(sub.ticker.clone(), pts.clone());
            }
            continue;
        }

        let source = config
            .custom_data_sources
            .iter()
            .find(|s| s.name() == sub.source_type)
            .cloned();
        let points = load_custom_data_points_for_subscription(
            config.data_root.clone(),
            sub.source_type.clone(),
            sub.ticker.clone(),
            current_date,
            source,
            sub.config.clone(),
            sub.dynamic_query.clone(),
        )
        .await
        .with_context(|| {
            format!(
                "failed to load custom data for {}/{} {}",
                sub.source_type, sub.ticker, current_date
            )
        })?;
        if !points.is_empty() {
            custom_data_for_day.insert(sub.ticker.clone(), points);
        }
    }
    Ok(custom_data_for_day)
}

async fn load_low_resolution_universe_data_for_day(
    custom_subs: &[CustomDataSubscription],
    config: &RunConfig,
    current_date: NaiveDate,
) -> Result<HashMap<String, Vec<CustomDataPoint>>> {
    let mut universe_data_for_day: HashMap<String, Vec<CustomDataPoint>> = HashMap::new();
    for sub in custom_subs {
        if !sub.is_universe() || sub.config.resolution.is_intraday() {
            continue;
        }
        let source = config
            .custom_data_sources
            .iter()
            .find(|s| s.name() == sub.source_type)
            .cloned();
        let points = load_universe_data_points_for_subscription(
            config.data_root.clone(),
            sub.source_type.clone(),
            sub.ticker.clone(),
            current_date,
            source,
            sub.config.clone(),
        )
        .await
        .with_context(|| {
            format!(
                "failed to load universe data for {}/{} {}",
                sub.source_type, sub.ticker, current_date
            )
        })?;
        if !points.is_empty() {
            universe_data_for_day.insert(sub.ticker.clone(), points);
        }
    }
    Ok(universe_data_for_day)
}

fn bucket_high_resolution_custom_points_by_end_time(
    buckets: &mut HashMap<i64, HashMap<String, Vec<CustomDataPoint>>>,
    ticker: &str,
    points: impl IntoIterator<Item = CustomDataPoint>,
) {
    for point in points {
        let Some(timestamp) = point.end_time else {
            continue;
        };
        buckets
            .entry(timestamp.0)
            .or_default()
            .entry(ticker.to_string())
            .or_default()
            .push(point);
    }
}

async fn load_custom_data_points_for_subscription(
    data_root: PathBuf,
    source_type: String,
    ticker: String,
    date: NaiveDate,
    source: Option<Arc<dyn lean_data_providers::ICustomDataSource>>,
    config: CustomDataConfig,
    dynamic_query: lean_data::CustomDataQuery,
) -> Result<Vec<CustomDataPoint>> {
    Ok(load_custom_data_points_for_subscription_with_status(
        data_root,
        source_type,
        ticker,
        date,
        source,
        config,
        dynamic_query,
    )
    .await?
    .points)
}

async fn load_universe_data_points_for_subscription(
    data_root: PathBuf,
    source_type: String,
    ticker: String,
    date: NaiveDate,
    source: Option<Arc<dyn lean_data_providers::ICustomDataSource>>,
    config: CustomDataConfig,
) -> Result<Vec<CustomDataPoint>> {
    Ok(load_custom_data_points_for_subscription_with_status(
        data_root,
        source_type,
        ticker,
        date,
        source,
        config,
        lean_data::CustomDataQuery::default(),
    )
    .await?
    .points)
}

struct CustomDataLoadResult {
    points: Vec<CustomDataPoint>,
    source_available: bool,
}

async fn load_live_custom_data_points_for_subscription_with_status(
    data_root: PathBuf,
    source_type: String,
    ticker: String,
    utc_time: DateTime,
    source: Arc<dyn lean_data_providers::ICustomDataSource>,
    mut config: CustomDataConfig,
    dynamic_query: lean_data::CustomDataQuery,
) -> Result<CustomDataLoadResult> {
    let data_root_string = data_root.display().to_string();
    config
        .properties
        .entry("data_root".to_string())
        .or_insert_with(|| data_root_string.clone());
    config
        .query
        .properties
        .entry("data_root".to_string())
        .or_insert_with(|| data_root_string.clone());

    let effective_query = config.query.merge(&dynamic_query);
    config.query = effective_query.clone();
    config.properties.extend(
        effective_query
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    let source_for_task = Arc::clone(&source);
    let ticker_for_task = ticker.clone();
    let config_for_task = config.clone();
    let query_for_task = effective_query.clone();
    let parquet_source = tokio::task::spawn_blocking(move || {
        source_for_task.get_live_parquet_source(
            &ticker_for_task,
            utc_time,
            &config_for_task,
            &query_for_task,
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("live custom parquet source task failed: {e}"))?;

    let Some(parquet_source) = parquet_source else {
        return Ok(CustomDataLoadResult {
            points: Vec::new(),
            source_available: false,
        });
    };

    let points = ParquetReader::new()
        .read_custom_parquet_points(&parquet_source, &effective_query, utc_time.date_utc())
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "live custom data read failed for {source_type}:{ticker}: {}",
                e
            )
        })?;
    Ok(CustomDataLoadResult {
        points,
        source_available: true,
    })
}

async fn load_custom_data_points_for_subscription_with_status(
    data_root: PathBuf,
    source_type: String,
    ticker: String,
    date: NaiveDate,
    source: Option<Arc<dyn lean_data_providers::ICustomDataSource>>,
    mut config: CustomDataConfig,
    dynamic_query: lean_data::CustomDataQuery,
) -> Result<CustomDataLoadResult> {
    let data_root_string = data_root.display().to_string();
    config
        .properties
        .entry("data_root".to_string())
        .or_insert_with(|| data_root_string.clone());
    config
        .query
        .properties
        .entry("data_root".to_string())
        .or_insert_with(|| data_root_string.clone());

    let effective_query = config.query.merge(&dynamic_query);
    config.query = effective_query.clone();
    config.properties.extend(
        effective_query
            .properties
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    if let Some(source_ref) = source.as_ref() {
        if source_ref.is_full_history_source() {
            let history_path = custom_data_history_path(&data_root, &source_type, &ticker);
            let all_points: Vec<CustomDataPoint> = if history_path.exists() {
                let hp = history_path.clone();
                tokio::task::spawn_blocking(move || {
                    ParquetReader::new()
                        .read_custom_data_points(&hp)
                        .unwrap_or_default()
                })
                .await
                .unwrap_or_default()
            } else {
                let data_source = match source_ref.get_source(
                    &ticker,
                    NaiveDate::from_ymd_opt(2000, 1, 1).unwrap(),
                    &config,
                ) {
                    Some(s) => s,
                    None => {
                        return Ok(CustomDataLoadResult {
                            points: Vec::new(),
                            source_available: false,
                        })
                    }
                };
                let raw = match data_source.transport {
                    CustomDataTransport::Http => {
                        let output = tokio::process::Command::new("curl")
                            .args(["-s", "--max-time", "120", "-L", &data_source.uri])
                            .output()
                            .await;
                        match output {
                            Ok(out) if out.status.success() => {
                                String::from_utf8_lossy(&out.stdout).to_string()
                            }
                            Ok(out) => {
                                warn!(
                                    "custom data full-history curl failed for {}/{}: {}",
                                    source_type,
                                    ticker,
                                    String::from_utf8_lossy(&out.stderr)
                                );
                                return Ok(CustomDataLoadResult {
                                    points: Vec::new(),
                                    source_available: false,
                                });
                            }
                            Err(e) => {
                                warn!(
                                    "custom data full-history download failed for {}/{}: {}",
                                    source_type, ticker, e
                                );
                                return Ok(CustomDataLoadResult {
                                    points: Vec::new(),
                                    source_available: false,
                                });
                            }
                        }
                    }
                    CustomDataTransport::LocalFile => {
                        match std::fs::read_to_string(&data_source.uri) {
                            Ok(text) => text,
                            Err(e) => {
                                warn!(
                                    "custom data local file read failed for {}/{}: {}",
                                    source_type, ticker, e
                                );
                                return Ok(CustomDataLoadResult {
                                    points: Vec::new(),
                                    source_available: false,
                                });
                            }
                        }
                    }
                };
                let source_clone = Arc::clone(source_ref);
                let cfg_clone = config.clone();
                let pts: Vec<CustomDataPoint> = tokio::task::spawn_blocking(move || {
                    raw.lines()
                        .filter_map(|line| source_clone.read_history_line(line, &cfg_clone))
                        .collect()
                })
                .await
                .unwrap_or_default();
                if !pts.is_empty() {
                    let hp = history_path.clone();
                    let pts_clone = pts.clone();
                    tokio::task::spawn_blocking(move || {
                        if let Some(parent) = hp.parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = ParquetWriter::new(WriterConfig {
                            bloom_filter: false,
                            write_statistics: false,
                            ..WriterConfig::default()
                        })
                        .write_custom_data_points(&pts_clone, &hp);
                    })
                    .await
                    .ok();
                }
                pts
            };
            return Ok(CustomDataLoadResult {
                points: all_points
                    .into_iter()
                    .filter(|point| point.time == date)
                    .collect(),
                source_available: true,
            });
        }

        let source_for_task = Arc::clone(source_ref);
        let ticker_for_task = ticker.clone();
        let config_for_task = config.clone();
        let query_for_task = effective_query.clone();
        let parquet_source = tokio::task::spawn_blocking(move || {
            source_for_task.get_parquet_source(
                &ticker_for_task,
                date,
                &config_for_task,
                &query_for_task,
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("custom parquet source task failed: {e}"))?;

        if let Some(parquet_source) = parquet_source {
            let points = ParquetReader::new()
                .read_custom_parquet_points(&parquet_source, &effective_query, date)
                .await
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            return Ok(CustomDataLoadResult {
                points,
                source_available: true,
            });
        }
        if source_ref.is_parquet_native() {
            return Ok(CustomDataLoadResult {
                points: Vec::new(),
                source_available: false,
            });
        }
    }

    let points = tokio::task::spawn_blocking(move || {
        load_custom_data_points(
            &data_root,
            &source_type,
            &ticker,
            date,
            source.as_ref(),
            &config,
        )
    })
    .await
    .unwrap_or_default();
    let source_available = !points.is_empty();
    Ok(CustomDataLoadResult {
        points,
        source_available,
    })
}

fn load_custom_data_points(
    data_root: &Path,
    source_type: &str,
    ticker: &str,
    date: NaiveDate,
    source: Option<&Arc<dyn lean_data_providers::ICustomDataSource>>,
    config: &CustomDataConfig,
) -> Vec<CustomDataPoint> {
    let cache_path = custom_data_path(data_root, source_type, ticker, date);

    // Cache hit — read and return.
    if cache_path.exists() {
        let reader = ParquetReader::new();
        return reader
            .read_custom_data_points(&cache_path)
            .unwrap_or_default();
    }

    // Cache miss — need a source plugin to fetch.
    let Some(source) = source else {
        return vec![];
    };

    // Ask plugin where to fetch data for this date.
    let data_source = match source.get_source(ticker, date, config) {
        Some(s) => s,
        None => return vec![], // no data for this date (e.g. weekend)
    };

    // Fetch raw content.
    let raw_content = match data_source.transport {
        CustomDataTransport::Http => {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .user_agent("Mozilla/5.0 (compatible; rlean/0.1)")
                .build()
                .unwrap_or_default();
            match client.get(&data_source.uri).send() {
                Ok(resp) => match resp.text() {
                    Ok(text) => text,
                    Err(e) => {
                        warn!(
                            "custom data fetch body error for {}/{} {}: {}",
                            source_type, ticker, date, e
                        );
                        return vec![];
                    }
                },
                Err(e) => {
                    warn!(
                        "custom data HTTP fetch failed for {}/{} {}: {}",
                        source_type, ticker, date, e
                    );
                    return vec![];
                }
            }
        }
        CustomDataTransport::LocalFile => match std::fs::read_to_string(&data_source.uri) {
            Ok(content) => content,
            Err(e) => {
                warn!(
                    "custom data file read failed for {}/{} {}: {}",
                    source_type, ticker, date, e
                );
                return vec![];
            }
        },
    };

    // Parse content using the plugin's reader() method.
    let mut points: Vec<CustomDataPoint> = Vec::new();
    match data_source.format {
        CustomDataFormat::Csv => {
            // Line-by-line: call reader() on each non-empty line.
            for line in raw_content.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(point) = source.reader(line, date, config) {
                    points.push(point);
                }
            }
        }
        CustomDataFormat::Json => {
            // Try to parse as JSON array; call reader() on each serialized element.
            match serde_json::from_str::<serde_json::Value>(&raw_content) {
                Ok(serde_json::Value::Array(arr)) => {
                    for elem in arr {
                        let line = elem.to_string();
                        if let Some(point) = source.reader(&line, date, config) {
                            points.push(point);
                        }
                    }
                }
                Ok(obj) => {
                    // Single JSON object — pass it as a single "line".
                    if let Some(point) = source.reader(&obj.to_string(), date, config) {
                        points.push(point);
                    }
                }
                Err(e) => {
                    warn!(
                        "custom data JSON parse error for {}/{} {}: {}",
                        source_type, ticker, date, e
                    );
                }
            }
        }
    }

    if points.is_empty() {
        return vec![];
    }

    // Write to Parquet cache (bloom filters AND page statistics disabled —
    // parquet-rs 53.x reader panics on TType::Set in metadata with these features).
    let writer = ParquetWriter::new(WriterConfig {
        bloom_filter: false,
        write_statistics: false,
        ..WriterConfig::default()
    });
    if let Err(e) = writer.write_custom_data_points(&points, &cache_path) {
        warn!(
            "failed to cache custom data for {}/{} {}: {}",
            source_type, ticker, date, e
        );
    }

    points
}

/// Apply a factor-file adjustment to a raw bar.
///
/// Looks up `(price_factor, split_factor)` for `bar_date` and scales
/// all OHLCV fields by `price_factor * split_factor`.  Volume is scaled
/// inversely (more shares at lower prices after a split).
/// Return the `(price_factor, split_factor)` that applies to `bar_date`.
///
/// Mirrors the LEAN behavior exercised by the tests here: use the most recent
/// Return the mapped ticker at `date` using LEAN's convention.
///
/// LEAN map files are interpreted as rows sorted by ascending date where each
/// row date is the last date the row's ticker is valid. The active mapping is
/// the first row whose date is on/after the query date.
fn ticker_at_date(rows: &[MapFileEntry], date: NaiveDate) -> Option<&str> {
    rows.iter()
        .filter(|r| r.date >= date)
        .min_by_key(|r| r.date)
        .map(|r| r.ticker.as_str())
}

/// Return the delisting date from a map file (last / newest row), if the
/// security is delisted.
///
/// LEAN convention: if the latest row's date is before 2049, it is a real
/// delisting date.  Far-future sentinels (year >= 2049) indicate active.
fn delisting_date(rows: &[MapFileEntry]) -> Option<NaiveDate> {
    rows.iter()
        .map(|r| r.date)
        .max()
        .filter(|d| d.year() < 2049)
}

fn first_map_file_date(rows: &[MapFileEntry]) -> Option<NaiveDate> {
    rows.iter().map(|r| r.date).min()
}

fn mapped_data_date_range(
    rows: &[MapFileEntry],
    requested_start: NaiveDate,
    requested_end: NaiveDate,
) -> Option<(NaiveDate, NaiveDate)> {
    let start = first_map_file_date(rows)
        .map(|first| requested_start.max(first))
        .unwrap_or(requested_start);
    let end = delisting_date(rows)
        .map(|delisted| requested_end.min(delisted))
        .unwrap_or(requested_end);

    if start <= end {
        Some((start, end))
    } else {
        None
    }
}

fn symbol_with_mapped_ticker(symbol: &Symbol, mapped_ticker: &str) -> Symbol {
    let mut mapped = symbol.clone();
    let ticker = mapped_ticker.to_uppercase();
    mapped.value = ticker.clone();
    mapped.permtick = ticker;
    mapped
}

fn mapped_symbol_for_provider(symbol: Symbol, rows: &[MapFileEntry], date: NaiveDate) -> Symbol {
    match ticker_at_date(rows, date) {
        Some(ticker) => symbol_with_mapped_ticker(&symbol, ticker),
        None => symbol,
    }
}

fn mapped_ticker_ranges(
    rows: &[MapFileEntry],
    start: NaiveDate,
    end: NaiveDate,
    default_ticker: &str,
) -> Vec<(NaiveDate, NaiveDate, String)> {
    if start > end {
        return Vec::new();
    }
    if rows.is_empty() {
        return vec![(start, end, default_ticker.to_uppercase())];
    }

    let mut ranges = Vec::new();
    let mut current_start = start;
    let mut current_ticker = ticker_at_date(rows, start)
        .unwrap_or(default_ticker)
        .to_uppercase();
    let mut date = start + chrono::Duration::days(1);
    while date <= end {
        let ticker = ticker_at_date(rows, date)
            .unwrap_or(default_ticker)
            .to_uppercase();
        if ticker != current_ticker {
            ranges.push((
                current_start,
                date - chrono::Duration::days(1),
                current_ticker,
            ));
            current_start = date;
            current_ticker = ticker;
        }
        date += chrono::Duration::days(1);
    }
    ranges.push((current_start, end, current_ticker));
    ranges
}

fn factor_file_covers_range(rows: &[FactorFileEntry], start: NaiveDate, end: NaiveDate) -> bool {
    if rows.is_empty() {
        return false;
    }
    let newest = rows.iter().map(|r| r.date).max();
    let oldest = rows.iter().map(|r| r.date).min();
    let lean_end_of_time = NaiveDate::from_ymd_opt(2050, 12, 31).unwrap();
    let identity_only = rows.iter().all(|r| {
        (r.price_factor - 1.0).abs() <= 1e-12
            && (r.split_factor - 1.0).abs() <= 1e-12
            && r.reference_price.abs() <= 1e-12
    });
    let current_date_placeholder =
        identity_only && rows.len() <= 2 && rows.iter().all(|r| r.date < lean_end_of_time);
    matches!((newest, oldest), (Some(n), Some(o)) if n >= end && o <= start && !current_date_placeholder)
}

fn split_event_for_date(
    symbol: &Symbol,
    rows: &[FactorFileEntry],
    date: NaiveDate,
    time: DateTime,
) -> Option<Split> {
    let (_, sf_today) = factor_for_entry(rows, date);
    let (_, sf_prev) = factor_for_entry(rows, date - chrono::Duration::days(1));
    if sf_today <= 0.0 || sf_prev <= 0.0 || (sf_today - sf_prev).abs() <= 1e-9 {
        return None;
    }

    // C# LEAN's SplitFactor is the price scale. Since rlean factor rows store
    // cumulative price split factors, the event factor is previous / current.
    let split_factor = sf_prev / sf_today;
    if (split_factor - 1.0).abs() <= 1e-9 {
        return None;
    }

    let reference_price = rows
        .iter()
        .filter(|row| row.date < date)
        .max_by_key(|row| row.date)
        .map(|row| row.reference_price)
        .unwrap_or(0.0);

    Some(Split::new(
        symbol.clone(),
        time,
        Decimal::from_f64(split_factor).unwrap_or(Decimal::ONE),
        Decimal::from_f64(reference_price).unwrap_or(Decimal::ZERO),
        SplitType::SplitOccurred,
    ))
}

fn split_events_for_date(
    subscriptions: &[Arc<SubscriptionDataConfig>],
    factor_map: &HashMap<u64, Vec<FactorFileEntry>>,
    date: NaiveDate,
    time: DateTime,
) -> Vec<Split> {
    let mut seen = HashSet::new();
    let mut splits = Vec::new();
    for sub in subscriptions {
        let sid = sub.symbol.id.sid;
        if !seen.insert(sid) {
            continue;
        }
        if let Some(rows) = factor_map.get(&sid) {
            if let Some(split) = split_event_for_date(&sub.symbol, rows, date, time) {
                splits.push(split);
            }
        }
    }
    splits
}

fn apply_split_events_to_state(
    splits: &[Split],
    subscriptions: &[Arc<SubscriptionDataConfig>],
    raw_equity_sids: &HashSet<u64>,
    portfolio: &lean_algorithm::portfolio::SecurityPortfolioManager,
    order_processor: &OrderProcessor,
    trade_builder: &mut TradeBuilder,
    brokerage_name: BrokerageName,
) {
    for split in splits {
        let sid = split.symbol.id.sid;
        let should_apply = subscriptions.iter().any(|sub| {
            sub.symbol.id.sid == sid
                && (sub.normalization_mode == DataNormalizationMode::Raw
                    || raw_equity_sids.contains(&sid))
        });
        if !should_apply {
            continue;
        }

        let before = portfolio.get_holding(&split.symbol);
        portfolio.apply_split(
            &split.symbol,
            split.split_factor,
            split.reference_price,
            Some(before.last_price * split.split_factor),
        );
        if brokerage_name == BrokerageName::TradierBrokerage && split.split_factor > Decimal::ONE {
            order_processor
                .transaction_manager
                .cancel_open_orders_for_symbol(sid, split.time);
        } else {
            order_processor
                .transaction_manager
                .apply_split_to_open_orders(sid, split.split_factor);
        }

        trade_builder.apply_split(&split.symbol, split.split_factor);

        let after = portfolio.get_holding(&split.symbol);
        if before.is_invested() {
            info!(
                "Split adjustment: {} factor {:.6}: qty {}→{} avg_px {}→{}",
                split.symbol.value,
                split.split_factor,
                before.quantity,
                after.quantity,
                before.average_price,
                after.average_price,
            );
        }
    }
}

/// factor-file row whose date is strictly earlier than `bar_date`.
/// If no such row exists, return `(1.0, 1.0)`.
fn factor_for_entry(rows: &[FactorFileEntry], bar_date: NaiveDate) -> (f64, f64) {
    if rows.is_empty() {
        return (1.0, 1.0);
    }
    // Most-recent row strictly before bar_date.
    if let Some(row) = rows
        .iter()
        .filter(|r| r.date < bar_date)
        .max_by_key(|r| r.date)
    {
        return (row.price_factor, row.split_factor);
    }
    // bar_date predates every row in the factor file.  Extend the oldest
    // cumulative factor backwards so there is no price discontinuity when the
    // backtest crosses into the period covered by the factor file.
    // Using (1.0, 1.0) here would cause a sudden apparent loss the moment the
    // first factor row became active (holdings bought at raw prices would be
    // re-marked at split/dividend-adjusted prices).
    if let Some(row) = rows.iter().min_by_key(|r| r.date) {
        return (row.price_factor, row.split_factor);
    }
    (1.0, 1.0)
}

fn factor_price_and_volume_scale(
    rows: &[FactorFileEntry],
    bar_date: NaiveDate,
) -> Option<(Decimal, Decimal)> {
    let (pf, sf) = factor_for_entry(rows, bar_date);
    let combined = pf * sf;
    if (combined - 1.0).abs() < 1e-9 {
        return None;
    }

    let price_scale = Decimal::from_f64(combined).unwrap_or(Decimal::ONE);
    let volume_scale = if combined.abs() > 1e-12 {
        Decimal::from_f64(1.0 / combined).unwrap_or(Decimal::ONE)
    } else {
        Decimal::ONE
    };
    Some((price_scale, volume_scale))
}

fn apply_factor_row(mut bar: TradeBar, rows: &[FactorFileEntry], bar_date: NaiveDate) -> TradeBar {
    let (pf, sf) = factor_for_entry(rows, bar_date);
    let combined = pf * sf;
    if (combined - 1.0).abs() < 1e-9 {
        return bar;
    } // fast-path: no adjustment

    let scale = Decimal::from_f64(combined).unwrap_or(Decimal::ONE);
    bar.open *= scale;
    bar.high *= scale;
    bar.low *= scale;
    bar.close *= scale;
    // Volume scales inversely to price for splits (more shares outstanding).
    if sf != 0.0 && (sf - 1.0).abs() > 1e-9 {
        let vol_scale = Decimal::from_f64(1.0 / sf).unwrap_or(Decimal::ONE);
        bar.volume *= vol_scale;
    }
    bar
}

fn apply_factor_quote_bar(
    mut bar: QuoteBar,
    rows: &[FactorFileEntry],
    bar_date: NaiveDate,
) -> QuoteBar {
    let Some((price_scale, volume_scale)) = factor_price_and_volume_scale(rows, bar_date) else {
        return bar;
    };

    if let Some(bid) = &mut bar.bid {
        bid.open *= price_scale;
        bid.high *= price_scale;
        bid.low *= price_scale;
        bid.close *= price_scale;
    }
    if let Some(ask) = &mut bar.ask {
        ask.open *= price_scale;
        ask.high *= price_scale;
        ask.low *= price_scale;
        ask.close *= price_scale;
    }
    bar.last_bid_size *= volume_scale;
    bar.last_ask_size *= volume_scale;
    bar
}

fn apply_factor_tick(mut tick: Tick, rows: &[FactorFileEntry], bar_date: NaiveDate) -> Tick {
    let Some((price_scale, volume_scale)) = factor_price_and_volume_scale(rows, bar_date) else {
        return tick;
    };

    match tick.tick_type {
        TickType::Trade => {
            tick.value *= price_scale;
            tick.quantity *= volume_scale;
        }
        TickType::Quote => {
            if tick.bid_price > Decimal::ZERO {
                tick.bid_price *= price_scale;
            }
            if tick.ask_price > Decimal::ZERO {
                tick.ask_price *= price_scale;
            }
            tick.bid_size *= volume_scale;
            tick.ask_size *= volume_scale;
            tick.value = if tick.bid_price > Decimal::ZERO && tick.ask_price > Decimal::ZERO {
                (tick.bid_price + tick.ask_price) / Decimal::from(2)
            } else if tick.bid_price > Decimal::ZERO {
                tick.bid_price
            } else {
                tick.ask_price
            };
        }
        TickType::OpenInterest => {}
    }
    tick
}

fn read_factor_rows_for_subscription(
    reader: &ParquetReader,
    data_root: &Path,
    sub: &SubscriptionDataConfig,
) -> lean_core::Result<Vec<FactorFileEntry>> {
    let ticker = sub.symbol.permtick.to_lowercase();
    let market = sub.symbol.market().as_str().to_lowercase();
    let sec = format!("{}", sub.symbol.security_type()).to_lowercase();
    let factor_path = data_root
        .join(&sec)
        .join(&market)
        .join("factor_files")
        .join(format!("{ticker}.parquet"));
    reader.read_factor_file(&factor_path)
}

fn map_file_path_for_subscription(data_root: &Path, sub: &SubscriptionDataConfig) -> PathBuf {
    let ticker = sub.symbol.permtick.to_lowercase();
    let market = sub.symbol.market().as_str().to_lowercase();
    data_root
        .join("equity")
        .join(&market)
        .join("map_files")
        .join(format!("{ticker}.parquet"))
}

fn ensure_map_rows_for_subscription(
    reader: &ParquetReader,
    data_root: &Path,
    sub: &SubscriptionDataConfig,
    map_file_map: &mut HashMap<u64, Vec<MapFileEntry>>,
    loaded_map_sids: &mut HashSet<u64>,
) {
    if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
        return;
    }
    if !loaded_map_sids.insert(sub.symbol.id.sid) {
        return;
    }

    let map_path = map_file_path_for_subscription(data_root, sub);
    match reader.read_map_file(&map_path) {
        Ok(rows) => {
            map_file_map.insert(sub.symbol.id.sid, rows);
        }
        Err(e) => {
            debug!("Skipping map file for {}: {}", sub.symbol.value, e);
        }
    }
}

fn subscription_has_mapped_data_for_range_from_rows(
    sub: &SubscriptionDataConfig,
    rows: &[MapFileEntry],
    start: NaiveDate,
    end: NaiveDate,
) -> bool {
    if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
        return true;
    }
    mapped_data_date_range(rows, start, end).is_some()
}

fn subscription_has_mapped_data_for_range_cached(
    map_file_map: &HashMap<u64, Vec<MapFileEntry>>,
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
) -> bool {
    if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
        return true;
    }
    map_file_map
        .get(&sub.symbol.id.sid)
        .is_some_and(|rows| subscription_has_mapped_data_for_range_from_rows(sub, rows, start, end))
}

fn subscription_has_mapped_data_for_range(
    reader: &ParquetReader,
    data_root: &Path,
    sub: &SubscriptionDataConfig,
    start: NaiveDate,
    end: NaiveDate,
) -> bool {
    if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
        return true;
    }
    let map_path = map_file_path_for_subscription(data_root, sub);
    let rows = reader.read_map_file(&map_path).unwrap_or_default();
    subscription_has_mapped_data_for_range_from_rows(sub, &rows, start, end)
}

fn load_factor_rows_into_map(
    reader: &ParquetReader,
    data_root: &Path,
    sub: &SubscriptionDataConfig,
    map_rows: Option<&[MapFileEntry]>,
    start: NaiveDate,
    end: NaiveDate,
    factor_map: &mut HashMap<u64, Vec<FactorFileEntry>>,
    require_factor_file: bool,
) -> Result<()> {
    if !matches!(sub.symbol.security_type(), SecurityType::Equity) {
        return Ok(());
    }
    let has_mapped_data = map_rows.map_or_else(
        || subscription_has_mapped_data_for_range(reader, data_root, sub, start, end),
        |rows| subscription_has_mapped_data_for_range_from_rows(sub, rows, start, end),
    );
    if !has_mapped_data {
        debug!(
            "Skipping factor file load for {}: requested range {} -> {} is outside map-file data range",
            sub.symbol.value, start, end
        );
        return Ok(());
    }

    match read_factor_rows_for_subscription(reader, data_root, sub) {
        Ok(rows) if !rows.is_empty() => {
            debug!("Loaded {} factor rows for {}", rows.len(), sub.symbol.value);
            factor_map.insert(sub.symbol.id.sid, rows);
            Ok(())
        }
        Ok(_) if require_factor_file => Err(anyhow::anyhow!(
            "Factor file for equity {} is empty after auxiliary generation",
            sub.symbol.value
        )),
        Err(e) if require_factor_file => Err(anyhow::anyhow!(
            "Factor file missing for equity {} after auxiliary generation: {}",
            sub.symbol.value,
            e
        )),
        _ => {
            warn!(
                "Factor file missing for {} — bars will not be adjusted.",
                sub.symbol.value
            );
            Ok(())
        }
    }
}

// ─── factor_for_entry unit tests — mirrors LEAN C# FactorFile.GetPriceFactor ─

#[cfg(test)]
mod factor_tests {
    use super::*;
    use chrono::NaiveDate;
    use lean_storage::schema::FactorFileEntry;
    use rust_decimal_macros::dec;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn entry(y: i32, m: u32, day: u32, pf: f64) -> FactorFileEntry {
        FactorFileEntry {
            date: d(y, m, day),
            price_factor: pf,
            split_factor: 1.0,
            reference_price: 0.0,
        }
    }

    fn entry_split(y: i32, m: u32, day: u32, pf: f64, sf: f64) -> FactorFileEntry {
        FactorFileEntry {
            date: d(y, m, day),
            price_factor: pf,
            split_factor: sf,
            reference_price: 0.0,
        }
    }

    /// Factor rows for `test_correctly_determines_price_factors`.
    ///
    /// rlean convention: a row at date D covers bars with `bar_date > D`
    /// (strict).  The factor applies to the BAR AFTER the row date, not on
    /// the row date itself.  This is the inverse of C# LEAN's CSV convention
    /// where the row date is the FIRST day the factor applies.
    ///
    /// Row layout (oldest → newest):
    ///   2023-01-14  pf=0.7  sf=0.125  → combined=0.0875  (split+div)
    ///   2023-10-16  pf=0.8  sf=0.25   → combined=0.2     (split)
    ///   2023-12-24  pf=0.8  sf=0.5    → combined=0.4     (split)
    ///   2023-12-31  pf=0.8  sf=1.0    → combined=0.8     (div)
    ///   2024-01-07  pf=0.9  sf=1.0    → combined=0.9     (div)
    ///   2050-12-31  pf=1.0  sf=1.0    → combined=1.0     (end-of-time sentinel)
    fn make_test_factor_rows() -> Vec<FactorFileEntry> {
        vec![
            entry_split(2023, 1, 14, 0.7, 0.125),
            entry_split(2023, 10, 16, 0.8, 0.25),
            entry_split(2023, 12, 24, 0.8, 0.5),
            entry_split(2023, 12, 31, 0.8, 1.0),
            entry_split(2024, 1, 7, 0.9, 1.0),
            entry_split(2050, 12, 31, 1.0, 1.0),
        ]
    }

    /// SPY-like factor file: entries starting 2021-03-25.
    fn spy_rows() -> Vec<FactorFileEntry> {
        vec![
            entry(2021, 3, 25, 0.9339743),
            entry(2021, 6, 17, 0.9339743),
            entry(2021, 9, 16, 0.9370296),
            entry(2021, 12, 16, 0.9400318),
            entry(2022, 3, 17, 0.9433413),
            entry(2026, 4, 9, 1.0),
        ]
    }

    /// Bar before the first factor file entry → returns oldest row's factor (backward extension).
    /// Previously returned (1.0, 1.0), which caused a phantom loss the moment the backtest
    /// crossed into the period covered by the factor file (prices would suddenly drop).
    #[test]
    fn test_before_first_entry_returns_oldest_factor() {
        let rows = spy_rows();
        let (pf, sf) = factor_for_entry(&rows, d(2020, 10, 16));
        assert!(
            (pf - 0.9339743).abs() < 1e-7,
            "bars before the factor file must extend the oldest factor backward (not 1.0)"
        );
        assert_eq!(sf, 1.0);
    }

    /// Bar exactly on the first entry date → no row strictly before it, so returns oldest
    /// row's factor (same backward-extension path as pre-first-row bars).
    #[test]
    fn test_on_first_entry_date_returns_oldest_factor() {
        let rows = spy_rows();
        let (pf, sf) = factor_for_entry(&rows, d(2021, 3, 25));
        assert!(
            (pf - 0.9339743).abs() < 1e-7,
            "bar on first entry date: no prior row exists, returns oldest row factor"
        );
        assert_eq!(sf, 1.0);
    }

    /// Bar one day after the first entry → picks the first entry's factor.
    #[test]
    fn test_day_after_first_entry() {
        let rows = spy_rows();
        let (pf, sf) = factor_for_entry(&rows, d(2021, 3, 26));
        assert!((pf - 0.9339743).abs() < 1e-7);
        assert_eq!(sf, 1.0);
    }

    /// Bar between two entries → picks the preceding (lower-date) entry.
    #[test]
    fn test_between_entries_picks_preceding() {
        let rows = spy_rows();
        // Between 2021-09-16 (0.9370296) and 2021-12-16 (0.9400318)
        let (pf, _) = factor_for_entry(&rows, d(2021, 11, 1));
        assert!(
            (pf - 0.9370296).abs() < 1e-7,
            "should pick the Sep-16 entry, not the Dec-16 one"
        );
    }

    /// Bar exactly on a non-first entry date → picks the preceding entry.
    #[test]
    fn test_on_middle_entry_date_picks_previous() {
        let rows = spy_rows();
        // On 2021-09-16 exactly → the Sep-16 row itself has date = bar_date,
        // so strict < excludes it; we get the Jun-17 entry (0.9339743).
        let (pf, _) = factor_for_entry(&rows, d(2021, 9, 16));
        assert!(
            (pf - 0.9339743).abs() < 1e-7,
            "bar ON an entry date picks the entry before it (strict <)"
        );
    }

    /// Bar after the last entry (2026-04-09) → picks the 2026-04-09 entry (factor=1.0).
    #[test]
    fn test_after_last_entry_picks_last() {
        let rows = spy_rows();
        let (pf, _) = factor_for_entry(&rows, d(2026, 4, 10));
        assert!(
            (pf - 1.0).abs() < 1e-9,
            "bars after the last entry get factor=1.0"
        );
    }

    /// Jan 4, 2022 must use the 2021-12-16 entry (0.9400318) — matches the
    /// observed LEAN C# value from the real SMA-crossover backtest log.
    #[test]
    fn test_jan_2022_matches_lean_observed() {
        let rows = spy_rows();
        let (pf, _) = factor_for_entry(&rows, d(2022, 1, 4));
        assert!(
            (pf - 0.9400318).abs() < 1e-7,
            "2022-01-04 must use the 2021-12-16 factor (0.9400318)"
        );
    }

    /// Empty rows → always 1.0.
    #[test]
    fn test_empty_rows() {
        assert_eq!(factor_for_entry(&[], d(2020, 1, 1)), (1.0, 1.0));
    }

    // ── C# LEAN parity tests ──────────────────────────────────────────────────
    //
    // These mirror the assertions in LEAN's FactorFileTests.CorrectlyDeterminesTimePriceFactors
    // (Lean/Tests/Common/Data/Auxiliary/FactorFileTests.cs) adapted to rlean's Parquet
    // convention.
    //
    // rlean convention vs C# CSV convention:
    //   C# row at date D → factor applies to bars WHERE bar_date >= D
    //   rlean row at date D → factor applies to bars WHERE bar_date > D  (strict)
    //
    // To obtain the same economic result, rlean row dates are one calendar day
    // EARLIER than the corresponding C# CSV row date.  E.g., a C# row at 2024-01-08
    // (dividend day) becomes a rlean row at 2024-01-07 (last pre-ex-div day).
    //
    // See make_test_factor_rows() for the data layout.

    /// Mirrors C# CorrectlyDeterminesTimePriceFactors.
    /// The combined price-scale factor (pf * sf) should match C#'s GetPriceFactor
    /// for the adjusted normalization mode.
    #[test]
    fn test_correctly_determines_price_factors() {
        let rows = make_test_factor_rows();

        // Helper: combined PSF for bar on the given date
        let psf = |y, m, day| {
            let (pf, sf) = factor_for_entry(&rows, d(y, m, day));
            pf * sf
        };

        // rlean convention: a row at date D applies to bars with bar_date > D (strictly).
        // Factor ranges (see make_test_factor_rows doc):
        //   bar > 2050-12-31 : 1.0   (sentinel kicks in)
        //   bar 2024-01-08..2050-12-31 : 0.9  (2024-01-07 row)
        //   bar 2024-01-01..2024-01-07 : 0.8  (2023-12-31 row)
        //   bar 2023-12-25..2023-12-31 : 0.4  (2023-12-24 row, split 2:1 applied)
        //   bar 2023-10-17..2023-12-24 : 0.2  (2023-10-16 row, split 4:1 applied)
        //   bar <= 2023-01-14          : 0.0875 (oldest row, backward extension)

        // After last real action (before sentinel) → still that action's factor
        assert!(
            (psf(2024, 1, 9) - 0.9).abs() < 1e-9,
            "bar after last action row → 0.9"
        );
        assert!(
            (psf(2024, 1, 8) - 0.9).abs() < 1e-9,
            "day after last action row → 0.9"
        );

        // ON the last action row date → falls back to prev row
        assert!(
            (psf(2024, 1, 7) - 0.8).abs() < 1e-9,
            "ON 2024-01-07 row → prev row 0.8"
        );

        // Between 2023-12-31 and 2024-01-07 rows → 2023-12-31 row (div, sf=1.0)
        assert!(
            (psf(2024, 1, 6) - 0.8).abs() < 1e-9,
            "2024-01-06 → 2023-12-31 row 0.8"
        );
        assert!(
            (psf(2024, 1, 1) - 0.8).abs() < 1e-9,
            "2024-01-01 → 2023-12-31 row 0.8"
        );

        // ON 2023-12-31 row → falls back to 2023-12-24 row (split 2:1, combined=0.4)
        assert!(
            (psf(2023, 12, 31) - 0.4).abs() < 1e-9,
            "ON 2023-12-31 row → 0.4"
        );

        // Between 2023-12-24 and 2023-12-31 rows → 2023-12-24 (split 2:1, combined=0.4)
        assert!(
            (psf(2023, 12, 30) - 0.4).abs() < 1e-9,
            "2023-12-30 → 2023-12-24 row 0.4"
        );
        assert!(
            (psf(2023, 12, 25) - 0.4).abs() < 1e-9,
            "2023-12-25 → 2023-12-24 row 0.4"
        );

        // ON 2023-12-24 row → falls to 2023-10-16 (split 4:1, combined=0.2)
        assert!(
            (psf(2023, 12, 24) - 0.2).abs() < 1e-9,
            "ON 2023-12-24 row → 0.2"
        );

        // Between 2023-10-16 and 2023-12-24 rows → 2023-10-16 row (split 4:1, combined=0.2)
        assert!(
            (psf(2023, 12, 1) - 0.2).abs() < 1e-9,
            "2023-12-01 → 2023-10-16 row 0.2"
        );
        assert!(
            (psf(2023, 10, 17) - 0.2).abs() < 1e-9,
            "2023-10-17 → 2023-10-16 row 0.2"
        );

        // ON 2023-10-16 row → falls to 2023-01-14 (oldest row, combined=0.0875)
        assert!(
            (psf(2023, 10, 16) - 0.0875).abs() < 1e-9,
            "ON 2023-10-16 row → 0.0875"
        );

        // Between first row and 2023-10-16 row → 2023-01-14 row (combined=0.0875)
        assert!(
            (psf(2023, 5, 1) - 0.0875).abs() < 1e-9,
            "2023-05-01 → first row 0.0875"
        );
        assert!(
            (psf(2023, 1, 15) - 0.0875).abs() < 1e-9,
            "day after first row → 0.0875"
        );

        // ON first row date and before → backward extension of oldest factor
        assert!(
            (psf(2023, 1, 14) - 0.0875).abs() < 1e-9,
            "ON first row → backward ext 0.0875"
        );
        assert!(
            (psf(2020, 1, 1) - 0.0875).abs() < 1e-9,
            "before first row → backward ext 0.0875"
        );
    }

    /// Mirrors C# HasSplitEventOnNextTradingDay.
    /// In rlean, a split row at date D (split_factor != 1) means the split
    /// took effect for bars dated D+1 and later.  The bar ON date D still
    /// uses the pre-split row (because `max(date < D)` = previous row).
    /// So the bar date when split first appears is D+1.
    #[test]
    fn test_split_detected_at_correct_bar_dates() {
        let rows = make_test_factor_rows();

        // 2023-12-24 row: sf=0.5 (2:1 split).  Appears for the first time on bar 2023-12-25.
        let (_, sf_before) = factor_for_entry(&rows, d(2023, 12, 24)); // ON row date → prev row
        let (_, sf_on_plus_one) = factor_for_entry(&rows, d(2023, 12, 25)); // 1 day after → this row
        assert!(
            (sf_before - 0.25).abs() < 1e-9,
            "day before split takes effect: sf=0.25"
        );
        assert!(
            (sf_on_plus_one - 0.5).abs() < 1e-9,
            "day split takes effect: sf=0.5"
        );

        // 2023-10-16 row: sf=0.25 (4:1 split).  First appears on bar 2023-10-17.
        let (_, sf_before2) = factor_for_entry(&rows, d(2023, 10, 16));
        let (_, sf_after2) = factor_for_entry(&rows, d(2023, 10, 17));
        assert!(
            (sf_before2 - 0.125).abs() < 1e-9,
            "before 4:1 split: sf=0.125"
        );
        assert!((sf_after2 - 0.25).abs() < 1e-9, "after 4:1 split: sf=0.25");
    }

    /// Mirrors C# HasDividendEventOnNextTradingDay.
    /// Dividend rows have sf=1.0; the price_factor drops to reflect the dividend.
    #[test]
    fn test_dividend_detected_at_correct_bar_dates() {
        let rows = make_test_factor_rows();

        // 2024-01-07 row: pf=0.9, sf=1.0 (dividend).
        // Bar on 2024-01-07 → uses prev row (pf=0.8), bar on 2024-01-08 → uses this row (pf=0.9).
        let (pf_on_row, _) = factor_for_entry(&rows, d(2024, 1, 7));
        let (pf_next_day, _) = factor_for_entry(&rows, d(2024, 1, 8));
        assert!(
            (pf_on_row - 0.8).abs() < 1e-9,
            "on div row date: still old pf=0.8"
        );
        assert!(
            (pf_next_day - 0.9).abs() < 1e-9,
            "day after div row: new pf=0.9"
        );
    }

    /// Split factor backward extension:  if the backtest starts before any split occurred,
    /// price continuity requires extending the oldest cumulative factor (which already
    /// encodes all historical splits) back to the dawn of time.
    #[test]
    fn test_split_factor_extends_backward_before_first_row() {
        let rows = make_test_factor_rows();
        // The oldest row (2023-01-14) has sf=0.125 (cumulative of all historical splits).
        // Bars from 1990 through 2023-01-14 must also see sf=0.125, not sf=1.0.
        let (pf, sf) = factor_for_entry(&rows, d(1990, 1, 1));
        assert!((pf - 0.7).abs() < 1e-9);
        assert!((sf - 0.125).abs() < 1e-9);
    }

    #[test]
    fn test_factor_file_coverage_rejects_sentinel_only_for_earlier_start() {
        let rows = vec![entry_split(2026, 4, 27, 1.0, 1.0)];

        assert!(!factor_file_covers_range(
            &rows,
            d(2022, 1, 1),
            d(2026, 3, 31)
        ));
    }

    #[test]
    fn test_factor_file_coverage_accepts_lean_identity_file() {
        let rows = vec![
            entry_split(2050, 12, 31, 1.0, 1.0),
            entry_split(1900, 1, 1, 1.0, 1.0),
        ];

        assert!(factor_file_covers_range(
            &rows,
            d(2022, 1, 1),
            d(2026, 3, 31)
        ));
    }

    #[test]
    fn test_factor_file_coverage_rejects_current_date_identity_placeholder() {
        let rows = vec![
            entry_split(2026, 4, 27, 1.0, 1.0),
            entry_split(1900, 1, 1, 1.0, 1.0),
        ];

        assert!(!factor_file_covers_range(
            &rows,
            d(2022, 1, 1),
            d(2026, 3, 31)
        ));
    }

    #[test]
    fn test_split_event_factor_matches_lean_price_scale() {
        let rows = vec![
            entry_split(2026, 1, 1, 1.0, 1.0),
            entry_split(2023, 6, 4, 1.0, 1.0),
            entry_split(1900, 1, 1, 1.0, 10.0),
        ];
        let symbol = lean_core::Symbol::create_equity("DPST", &lean_core::Market::usa());

        let split = split_event_for_date(
            &symbol,
            &rows,
            d(2023, 6, 5),
            lean_core::NanosecondTimestamp(0),
        )
        .expect("reverse split should be detected");

        assert_eq!(split.split_factor, dec!(10));
        assert_eq!(split.split_type, SplitType::SplitOccurred);
    }

    /// apply_factor_row: a 2:1 split (sf=0.5) on bar after row date halves the price
    /// and doubles the volume.
    #[test]
    fn test_apply_factor_row_scales_volume_for_split() {
        use lean_data::trade_bar::TradeBarData;
        use rust_decimal_macros::dec;

        // A 2:1 split (sf=0.5) should DOUBLE the volume on pre-split bars.
        let rows = vec![
            entry_split(2023, 12, 24, 1.0, 0.5),
            entry_split(2050, 12, 31, 1.0, 1.0),
        ];

        let sym = lean_core::Symbol::create_equity("SPY", &lean_core::Market::usa());
        let bar = TradeBar::new(
            sym,
            lean_core::NanosecondTimestamp(0),
            lean_core::TimeSpan::from_days(1),
            TradeBarData::new(dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000)),
        );

        // bar on 2023-12-25 (one day after the row): split factor 0.5 applies.
        let adjusted = apply_factor_row(bar.clone(), &rows, d(2023, 12, 25));
        assert_eq!(adjusted.close, dec!(52.5)); // 105 * 0.5
        assert_eq!(adjusted.volume, dec!(2000)); // 1000 / 0.5
    }

    #[test]
    fn test_apply_factor_quote_bar_prevents_raw_quote_adjusted_trade_mismatch() {
        use lean_data::quote_bar::Bar;
        use rust_decimal_macros::dec;

        let rows = vec![
            entry_split(2050, 12, 31, 1.0, 1.0),
            entry_split(1900, 1, 1, 1.0, 10.0),
        ];

        let sym = lean_core::Symbol::create_equity("SIRI", &lean_core::Market::usa());
        let qbar = QuoteBar::new(
            sym,
            lean_core::NanosecondTimestamp(0),
            lean_core::TimeSpan::from_mins(1),
            Some(Bar::new(dec!(6.60), dec!(6.60), dec!(6.60), dec!(6.60))),
            Some(Bar::new(dec!(6.61), dec!(6.61), dec!(6.61), dec!(6.61))),
            dec!(1000),
            dec!(1200),
        );

        let adjusted = apply_factor_quote_bar(qbar, &rows, d(2022, 3, 29));
        let synth = synthesize_trade_bar_from_quote_bar(&adjusted).unwrap();

        assert_eq!(adjusted.bid.as_ref().unwrap().close, dec!(66.00));
        assert_eq!(adjusted.ask.as_ref().unwrap().close, dec!(66.10));
        assert_eq!(synth.close, dec!(66.05));
    }

    #[test]
    fn test_apply_factor_tick_quote_prevents_raw_quote_fill() {
        use rust_decimal_macros::dec;

        let rows = vec![
            entry_split(2050, 12, 31, 1.0, 1.0),
            entry_split(1900, 1, 1, 1.0, 10.0),
        ];

        let sym = lean_core::Symbol::create_equity("SIRI", &lean_core::Market::usa());
        let tick = Tick::quote(
            sym.clone(),
            lean_core::NanosecondTimestamp(0),
            dec!(6.60),
            dec!(6.61),
            dec!(1000),
            dec!(1200),
        );

        let adjusted = apply_factor_tick(tick, &rows, d(2022, 3, 29));
        let synth = synthesize_trade_bar_from_ticks(
            &sym,
            lean_core::NanosecondTimestamp(0),
            std::slice::from_ref(&adjusted),
        )
        .unwrap();

        assert_eq!(adjusted.bid_price, dec!(66.00));
        assert_eq!(adjusted.ask_price, dec!(66.10));
        assert_eq!(adjusted.value, dec!(66.05));
        assert_eq!(synth.close, dec!(66.05));
    }
}

// ─── benchmark unit tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
    use lean_core::SymbolOptionsExt;
    use lean_data::quote_bar::Bar;
    use rust_decimal_macros::dec;

    /// Helper: create a fresh QcAlgorithm with default (no) benchmark.
    fn make_alg() -> QcAlgorithm {
        QcAlgorithm::new("test", dec!(100_000))
    }

    fn hyperliquid_future(ticker: &str) -> Symbol {
        Symbol::create_crypto_future(ticker, &Market::new(Market::HYPERLIQUID))
    }

    fn ny_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime {
        let local = lean_core::time::tz::NEW_YORK
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .unwrap();
        local.with_timezone(&chrono::Utc).into()
    }

    struct RecordingBrokerage {
        placed: Arc<std::sync::atomic::AtomicUsize>,
        updated: Arc<std::sync::atomic::AtomicUsize>,
        canceled: Arc<std::sync::atomic::AtomicUsize>,
        last_updated_order: Arc<Mutex<Option<lean_orders::Order>>>,
        account_orders: Arc<Mutex<Vec<lean_orders::Order>>>,
        connected: bool,
    }

    impl Brokerage for RecordingBrokerage {
        fn name(&self) -> &str {
            "Recording"
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn connect(&mut self) -> lean_core::Result<()> {
            self.connected = true;
            Ok(())
        }

        fn disconnect(&mut self) {
            self.connected = false;
        }

        fn place_order(&mut self, _order: lean_orders::Order) -> lean_core::Result<bool> {
            self.placed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(true)
        }

        fn place_order_with_brokerage_ids(
            &mut self,
            _order: lean_orders::Order,
        ) -> lean_core::Result<Option<Vec<String>>> {
            self.placed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Some(vec!["brokerage-1".to_string()]))
        }

        fn update_order(&mut self, order: &lean_orders::Order) -> lean_core::Result<bool> {
            self.updated
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *self.last_updated_order.lock().unwrap() = Some(order.clone());
            Ok(true)
        }

        fn can_update_order(
            &self,
            _order: &lean_orders::Order,
            request: &lean_orders::UpdateOrderRequest,
        ) -> bool {
            request
                .fields
                .quantity
                .map(|quantity| quantity == request.previous_order.quantity)
                .unwrap_or(true)
        }

        fn cancel_order(&mut self, _order: &lean_orders::Order) -> lean_core::Result<bool> {
            self.canceled
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(true)
        }

        fn get_open_orders(&self) -> Vec<lean_orders::Order> {
            Vec::new()
        }

        fn get_account_orders(&self) -> Vec<lean_orders::Order> {
            self.account_orders.lock().unwrap().clone()
        }

        fn get_cash_balance(&self) -> Vec<(String, lean_core::Price)> {
            Vec::new()
        }

        fn get_account_holdings(&self) -> HashMap<Symbol, lean_core::Quantity> {
            HashMap::new()
        }
    }

    #[test]
    fn live_brokerage_bridge_places_orders_in_paper_fill_mode() {
        let placed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brokerage = Box::new(RecordingBrokerage {
            placed: placed.clone(),
            updated: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            canceled: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_updated_order: Arc::new(Mutex::new(None)),
            account_orders: Arc::new(Mutex::new(Vec::new())),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, true).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        transactions.add_order(lean_orders::Order::market(
            1,
            symbol,
            dec!(1),
            DateTime::EPOCH,
            "",
        ));
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        let events = bridge
            .submit_new_orders(&processor, DateTime::EPOCH)
            .unwrap();

        assert_eq!(placed.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Submitted);
        assert!(events[0].message.contains("paper fill mode"));
        assert_eq!(
            processor
                .transaction_manager
                .get_order(1)
                .unwrap()
                .brokerage_id,
            vec!["brokerage-1".to_string()]
        );
    }

    #[test]
    fn local_paper_new_orders_emit_submitted_events_before_fills() {
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        transactions.add_order(lean_orders::Order::market(
            1,
            symbol.clone(),
            dec!(1),
            DateTime::EPOCH,
            "",
        ));
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        let events = drain_local_new_orders(&processor, DateTime::EPOCH, "local paper brokerage");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Submitted);
        assert_eq!(events[0].quantity, dec!(1));
        processor
            .transaction_manager
            .process_order_event(events[0].clone());
        assert_eq!(
            processor.transaction_manager.get_order(1).unwrap().status,
            OrderStatus::Submitted
        );

        let bar = TradeBar::new(
            symbol.clone(),
            DateTime::EPOCH,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(450), dec!(451), dec!(449), dec!(450), dec!(1000)),
        );
        let bars = HashMap::from([(symbol.id.sid, bar)]);
        let fills =
            processor.generate_order_events_with_quotes(&bars, &HashMap::new(), DateTime::EPOCH);

        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].status, OrderStatus::Filled);
        assert_eq!(fills[0].fill_quantity, dec!(1));
    }

    #[test]
    fn live_brokerage_bridge_places_orders_in_real_order_mode() {
        let placed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brokerage = Box::new(RecordingBrokerage {
            placed: placed.clone(),
            updated: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            canceled: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_updated_order: Arc::new(Mutex::new(None)),
            account_orders: Arc::new(Mutex::new(Vec::new())),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, false).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        transactions.add_order(lean_orders::Order::market(
            1,
            symbol,
            dec!(1),
            DateTime::EPOCH,
            "",
        ));
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        let events = bridge
            .submit_new_orders(&processor, DateTime::EPOCH)
            .unwrap();

        assert_eq!(placed.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Submitted);
        assert_eq!(events[0].message, "Recording accepted order");
        assert_eq!(
            processor
                .transaction_manager
                .get_order(1)
                .unwrap()
                .brokerage_id,
            vec!["brokerage-1".to_string()]
        );
    }

    #[test]
    fn live_brokerage_bridge_reconciles_real_order_fill_snapshots() {
        let placed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let account_orders = Arc::new(Mutex::new(Vec::new()));
        let brokerage = Box::new(RecordingBrokerage {
            placed,
            updated: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            canceled: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_updated_order: Arc::new(Mutex::new(None)),
            account_orders: account_orders.clone(),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, false).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        transactions.add_order(lean_orders::Order::market(
            1,
            symbol.clone(),
            dec!(1),
            DateTime::EPOCH,
            "",
        ));
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        let mut submitted_events = bridge
            .submit_new_orders(&processor, DateTime::EPOCH)
            .unwrap();
        for event in submitted_events.drain(..) {
            processor.transaction_manager.process_order_event(event);
        }

        let mut broker_order =
            lean_orders::Order::market(999, symbol, dec!(1), DateTime::EPOCH, "");
        broker_order.status = OrderStatus::Filled;
        broker_order.brokerage_id = vec!["brokerage-1".to_string()];
        broker_order.filled_quantity = dec!(1);
        broker_order.average_fill_price = dec!(450);
        *account_orders.lock().unwrap() = vec![broker_order];

        let events = bridge.reconcile_order_events(&processor, DateTime::EPOCH);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Filled);
        assert_eq!(events[0].fill_quantity, dec!(1));
        assert_eq!(events[0].fill_price, dec!(450));
        processor
            .transaction_manager
            .process_order_event(events[0].clone());
        let local_order = processor.transaction_manager.get_order(1).unwrap();
        assert_eq!(local_order.filled_quantity, dec!(1));
        assert_eq!(local_order.average_fill_price, dec!(450));
    }

    #[test]
    fn live_brokerage_bridge_routes_real_cancel_requests_to_brokerage() {
        let canceled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brokerage = Box::new(RecordingBrokerage {
            placed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            updated: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            canceled: canceled.clone(),
            last_updated_order: Arc::new(Mutex::new(None)),
            account_orders: Arc::new(Mutex::new(Vec::new())),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, false).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut order =
            lean_orders::Order::limit(1, symbol.clone(), dec!(1), dec!(450), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        order.brokerage_id = vec!["brokerage-1".to_string()];
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        assert!(processor.transaction_manager.request_cancel_order(
            1,
            DateTime::EPOCH,
            "cancel".to_string(),
        ));
        let events = bridge
            .process_cancel_requests(&processor, DateTime::EPOCH)
            .unwrap();

        assert_eq!(canceled.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Canceled);
        processor
            .transaction_manager
            .process_order_event(events[0].clone());
        let local_order = processor.transaction_manager.get_order(1).unwrap();
        assert_eq!(local_order.status, OrderStatus::Canceled);
        assert_eq!(local_order.canceled_time, Some(DateTime::EPOCH));
        assert!(processor
            .transaction_manager
            .get_cancel_requests()
            .is_empty());
    }

    #[test]
    fn live_brokerage_bridge_routes_paper_fill_cancel_requests_to_brokerage() {
        let canceled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brokerage = Box::new(RecordingBrokerage {
            placed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            updated: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            canceled: canceled.clone(),
            last_updated_order: Arc::new(Mutex::new(None)),
            account_orders: Arc::new(Mutex::new(Vec::new())),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, true).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut order =
            lean_orders::Order::limit(1, symbol.clone(), dec!(1), dec!(450), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        assert!(processor.transaction_manager.request_cancel_order(
            1,
            DateTime::EPOCH,
            "cancel".to_string(),
        ));
        let events = bridge
            .process_cancel_requests(&processor, DateTime::EPOCH)
            .unwrap();

        assert_eq!(canceled.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Canceled);
    }

    #[test]
    fn live_brokerage_bridge_routes_real_update_requests_to_brokerage() {
        let updated = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let last_updated_order = Arc::new(Mutex::new(None));
        let brokerage = Box::new(RecordingBrokerage {
            placed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            updated: updated.clone(),
            canceled: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_updated_order: last_updated_order.clone(),
            account_orders: Arc::new(Mutex::new(Vec::new())),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, false).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut order =
            lean_orders::Order::limit(1, symbol, dec!(1), dec!(450), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        order.brokerage_id = vec!["brokerage-1".to_string()];
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        assert!(processor.transaction_manager.request_update_order(
            1,
            DateTime::EPOCH,
            lean_orders::UpdateOrderFields {
                limit_price: Some(dec!(451)),
                ..Default::default()
            },
        ));
        let events = bridge
            .process_update_requests(&processor, DateTime::EPOCH)
            .unwrap();

        assert_eq!(updated.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            last_updated_order
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .limit_price,
            Some(dec!(451))
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::UpdateSubmitted);
        assert!(processor
            .transaction_manager
            .get_update_requests()
            .is_empty());
    }

    #[test]
    fn live_brokerage_bridge_rejects_quantity_update_before_brokerage_call() {
        let updated = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let brokerage = Box::new(RecordingBrokerage {
            placed: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            updated: updated.clone(),
            canceled: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            last_updated_order: Arc::new(Mutex::new(None)),
            account_orders: Arc::new(Mutex::new(Vec::new())),
            connected: false,
        });
        let mut bridge = LiveBrokerageBridge::connect(brokerage, false).unwrap();
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut order =
            lean_orders::Order::limit(1, symbol, dec!(1), dec!(450), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        order.brokerage_id = vec!["brokerage-1".to_string()];
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        assert!(processor.transaction_manager.request_update_order(
            1,
            DateTime::EPOCH,
            lean_orders::UpdateOrderFields {
                quantity: Some(dec!(2)),
                ..Default::default()
            },
        ));
        let events = bridge
            .process_update_requests(&processor, DateTime::EPOCH)
            .unwrap();

        assert_eq!(updated.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Submitted);
        assert!(events[0].message.contains("rejected update"));
        assert_eq!(
            processor.transaction_manager.get_order(1).unwrap().quantity,
            dec!(1)
        );
        assert!(processor
            .transaction_manager
            .get_update_requests()
            .is_empty());
    }

    #[test]
    fn tradier_paper_fill_filter_defers_regular_session_order_during_premarket() {
        let mut algorithm = QcAlgorithm::new("tradier-paper-fill-filter", dec!(100_000));
        algorithm.set_brokerage_model(BrokerageName::TradierBrokerage, AccountType::Margin);
        let symbol = algorithm.add_equity("SPY", Resolution::Minute);
        let time = ny_time(2026, 1, 16, 8, 0);
        let order_time = time - TimeSpan::ONE_MINUTE;
        algorithm.utc_time = time;

        let mut order =
            lean_orders::Order::limit(1, symbol.clone(), dec!(1), dec!(100), order_time, "");
        order.time_in_force = lean_orders::TimeInForce::Day;
        order.status = OrderStatus::Submitted;
        algorithm.transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            algorithm.transactions.clone(),
        );
        let bar = TradeBar::new(
            symbol.clone(),
            time,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(99), dec!(101), dec!(99), dec!(100), dec!(1000)),
        );
        let bars = HashMap::from([(symbol.id.sid, bar)]);

        let mut fill_events =
            processor.generate_order_events_with_quotes(&bars, &HashMap::new(), time);

        assert_eq!(fill_events.len(), 1);
        retain_brokerage_executable_paper_fills(&algorithm, &processor, &mut fill_events);
        assert!(fill_events.is_empty());
    }

    #[test]
    fn live_brokerage_model_config_applies_margin_model_before_initialize() {
        let mut algorithm = QcAlgorithm::new("live-model", dec!(100_000));

        apply_live_brokerage_model(&mut algorithm, Some(BrokerageName::TradierBrokerage));

        assert_eq!(algorithm.brokerage_name, BrokerageName::TradierBrokerage);
        assert_eq!(algorithm.account_type, AccountType::Margin);
    }

    #[test]
    fn live_brokerage_model_config_leaves_default_when_absent() {
        let mut algorithm = QcAlgorithm::new("live-model", dec!(100_000));

        apply_live_brokerage_model(&mut algorithm, None);

        assert_eq!(algorithm.brokerage_name, BrokerageName::Default);
        assert_eq!(algorithm.account_type, AccountType::Margin);
    }

    #[test]
    fn tradier_reverse_split_cancels_open_orders_instead_of_adjusting_them() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let split_time = ny_time(2026, 1, 16, 9, 30);
        let mut subscription =
            SubscriptionDataConfig::new_equity(symbol.clone(), Resolution::Minute);
        subscription.normalization_mode = DataNormalizationMode::Raw;
        let subscriptions = vec![Arc::new(subscription)];
        let portfolio = lean_algorithm::portfolio::SecurityPortfolioManager::new(dec!(100_000));
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let mut order =
            lean_orders::Order::limit(1, symbol.clone(), dec!(100), dec!(100), split_time, "");
        order.status = OrderStatus::Submitted;
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );
        let split = Split::new(
            symbol,
            split_time,
            dec!(10),
            dec!(50),
            SplitType::SplitOccurred,
        );
        let mut trade_builder = TradeBuilder::new();

        apply_split_events_to_state(
            &[split],
            &subscriptions,
            &HashSet::new(),
            &portfolio,
            &processor,
            &mut trade_builder,
            BrokerageName::TradierBrokerage,
        );

        let order = processor.transaction_manager.get_order(1).unwrap();
        assert_eq!(order.status, OrderStatus::Canceled);
        assert_eq!(order.canceled_time, Some(split_time));
        assert_eq!(order.quantity, dec!(100));
        assert_eq!(order.limit_price, Some(dec!(100)));
    }

    #[test]
    fn default_reverse_split_adjusts_open_orders() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let split_time = ny_time(2026, 1, 16, 9, 30);
        let mut subscription =
            SubscriptionDataConfig::new_equity(symbol.clone(), Resolution::Minute);
        subscription.normalization_mode = DataNormalizationMode::Raw;
        let subscriptions = vec![Arc::new(subscription)];
        let portfolio = lean_algorithm::portfolio::SecurityPortfolioManager::new(dec!(100_000));
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let mut order =
            lean_orders::Order::limit(1, symbol.clone(), dec!(100), dec!(100), split_time, "");
        order.status = OrderStatus::Submitted;
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );
        let split = Split::new(
            symbol,
            split_time,
            dec!(10),
            dec!(50),
            SplitType::SplitOccurred,
        );
        let mut trade_builder = TradeBuilder::new();

        apply_split_events_to_state(
            &[split],
            &subscriptions,
            &HashSet::new(),
            &portfolio,
            &processor,
            &mut trade_builder,
            BrokerageName::Default,
        );

        let order = processor.transaction_manager.get_order(1).unwrap();
        assert_eq!(order.status, OrderStatus::Submitted);
        assert_eq!(order.quantity, dec!(10));
        assert_eq!(order.limit_price, Some(dec!(1000)));
    }

    #[test]
    fn local_cancel_requests_emit_canceled_events_and_clear_request() {
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        transactions.add_order(lean_orders::Order::limit(
            1,
            symbol,
            dec!(1),
            dec!(450),
            DateTime::EPOCH,
            "",
        ));
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        assert!(processor.transaction_manager.request_cancel_order(
            1,
            DateTime::EPOCH,
            "cancel".to_string(),
        ));
        let events = drain_local_cancel_requests(&processor, DateTime::EPOCH, "test");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Canceled);
        assert!(processor
            .transaction_manager
            .get_cancel_requests()
            .is_empty());
        processor
            .transaction_manager
            .process_order_event(events[0].clone());
        assert_eq!(
            processor.transaction_manager.get_order(1).unwrap().status,
            OrderStatus::Canceled
        );
    }

    #[test]
    fn local_update_requests_emit_update_events_and_clear_request() {
        let transactions = Arc::new(lean_orders::transaction_manager::TransactionManager::new());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut order =
            lean_orders::Order::limit(1, symbol, dec!(1), dec!(450), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        transactions.add_order(order);
        let processor = OrderProcessor::new(
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
            transactions,
        );

        assert!(processor.transaction_manager.request_update_order(
            1,
            DateTime::EPOCH,
            lean_orders::UpdateOrderFields {
                limit_price: Some(dec!(451)),
                ..Default::default()
            },
        ));
        let events = drain_local_update_requests(&processor, DateTime::EPOCH, "test");

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::UpdateSubmitted);
        assert!(processor
            .transaction_manager
            .get_update_requests()
            .is_empty());
        assert_eq!(
            processor
                .transaction_manager
                .get_order(1)
                .unwrap()
                .limit_price,
            Some(dec!(451))
        );
    }

    #[test]
    fn initial_brokerage_account_state_seeds_portfolio_and_orders() {
        let algorithm = Arc::new(Mutex::new(QcAlgorithm::new(
            "initial-account-sync",
            dec!(100000),
        )));
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let open_order =
            lean_orders::Order::limit(42, symbol.clone(), dec!(5), dec!(450), DateTime::EPOCH, "");
        let account_state = AccountState {
            cash: dec!(12345),
            cash_balances: vec![("USD".to_string(), dec!(12345))],
            positions: HashMap::from([(symbol.id.ticker.clone(), dec!(10))]),
            holdings: vec![lean_brokerages::BrokerageHolding {
                symbol: symbol.clone(),
                quantity: dec!(10),
                average_price: dec!(400),
            }],
            open_orders: vec![open_order],
            last_sync_time: chrono::Utc::now(),
        };

        apply_initial_brokerage_account_state(&algorithm, &account_state);

        let algorithm = algorithm.lock().unwrap();
        assert_eq!(*algorithm.portfolio.cash.read(), dec!(12345));
        let holding = algorithm.portfolio.get_holding(&symbol);
        assert_eq!(holding.quantity, dec!(10));
        assert_eq!(holding.average_price, dec!(400));
        assert!(algorithm.securities.contains(&symbol));
        assert_eq!(
            algorithm.transactions.get_order(42).unwrap().quantity,
            dec!(5)
        );
        assert!(algorithm
            .subscription_manager
            .get_all()
            .iter()
            .any(|subscription| subscription.symbol == symbol));
    }

    #[test]
    fn live_fill_forward_adds_missing_hourly_quote_bar() {
        let symbol = hyperliquid_future("XYZ:KR200");
        let mut quote_config =
            SubscriptionDataConfig::new_crypto_future(symbol.clone(), Resolution::Hour);
        quote_config.tick_type = TickType::Quote;
        let subscriptions = vec![Arc::new(quote_config)];
        let mut state = LiveFillForwardState::default();
        let first_frontier = DateTime::from_secs(3_600);
        let first = QuoteBar::new(
            symbol.clone(),
            first_frontier - TimeSpan::ONE_HOUR,
            TimeSpan::ONE_HOUR,
            Some(Bar::from_price(dec!(100))),
            Some(Bar::from_price(dec!(102))),
            dec!(1),
            dec!(1),
        );
        let mut first_slice = Slice::new(first_frontier);
        first_slice.add_quote_bar(first);
        state.apply(&first_slice, &subscriptions);

        let second_frontier = first_frontier + TimeSpan::ONE_HOUR;
        let second_slice = Slice::new(second_frontier);
        let filled = state.apply(&second_slice, &subscriptions);
        let filled_bar = filled.quote_bars.get(&symbol.id.sid).unwrap();

        assert_eq!(filled_bar.time, first_frontier);
        assert_eq!(filled_bar.end_time, second_frontier);
        assert_eq!(filled_bar.period, TimeSpan::ONE_HOUR);
        assert_eq!(filled_bar.mid_close(), dec!(101));
    }

    #[test]
    fn dynamic_quote_bar_is_added_to_current_slice_with_trade_bar_present() {
        let symbol = hyperliquid_future("XYZ:COST");
        let sid = symbol.id.sid;
        let frontier = DateTime::from_secs(3_600);
        let trade_bar = TradeBar::new(
            symbol.clone(),
            frontier,
            TimeSpan::ONE_HOUR,
            TradeBarData::new(
                dec!(975.67),
                dec!(978.46),
                dec!(975.67),
                dec!(978.46),
                dec!(1),
            ),
        );
        let quote_bar = QuoteBar::new(
            symbol.clone(),
            frontier,
            TimeSpan::ONE_HOUR,
            Some(Bar::new(
                dec!(962.69),
                dec!(965.44),
                dec!(962.69),
                dec!(965.44),
            )),
            Some(Bar::new(
                dec!(983.26),
                dec!(986.07),
                dec!(983.26),
                dec!(986.07),
            )),
            Decimal::ZERO,
            Decimal::ZERO,
        );

        let mut bars_for_orders = HashMap::from([(sid, trade_bar.clone())]);
        let mut minute_quote_bars = HashMap::new();
        let mut minute_slice = Slice::new(frontier);
        let mut updated_mid_price = false;

        apply_quote_bar_to_minute(
            sid,
            quote_bar.clone(),
            frontier.date_utc(),
            &HashSet::new(),
            &HashMap::new(),
            &mut bars_for_orders,
            &mut minute_quote_bars,
            &mut minute_slice,
            |_, _, _, _, update_mid| updated_mid_price = update_mid,
        );

        assert_eq!(bars_for_orders.get(&sid).unwrap().open, trade_bar.open);
        assert!(!updated_mid_price);
        assert_eq!(
            minute_quote_bars
                .get(&sid)
                .unwrap()
                .bid
                .as_ref()
                .unwrap()
                .close,
            dec!(965.44)
        );
        assert!(minute_slice.quote_bars.contains_key(&sid));
    }

    #[test]
    fn live_universe_funding_rows_apply_to_crypto_future_cash() {
        let symbol = hyperliquid_future("XYZ:USAR");
        let portfolio = Arc::new(SecurityPortfolioManager::new(dec!(100_000)));
        portfolio.apply_fill_with_multiplier(&symbol, dec!(100), dec!(10), dec!(0), dec!(1));
        portfolio.update_prices(&symbol, dec!(100));

        let row_date = chrono::NaiveDate::from_ymd_opt(2026, 6, 13).unwrap();
        let mut fields = HashMap::new();
        fields.insert("symbol".to_string(), serde_json::json!("XYZ:USAR"));
        fields.insert(
            "security_type".to_string(),
            serde_json::json!("CryptoFuture"),
        );
        fields.insert("market".to_string(), serde_json::json!("hyperliquid"));
        fields.insert("funding".to_string(), serde_json::json!("0.0001"));
        let point = CustomDataPoint {
            time: row_date,
            end_time: None,
            value: dec!(100),
            fields,
        };

        assert_eq!(
            apply_live_universe_margin_interest_rates(
                &portfolio,
                DateTime::from_secs(0),
                std::slice::from_ref(&point)
            ),
            Decimal::ZERO
        );
        assert_eq!(
            apply_live_universe_margin_interest_rates(
                &portfolio,
                DateTime::from_secs(3_600),
                &[point]
            ),
            dec!(-0.1000)
        );
        assert_eq!(*portfolio.cash.read(), dec!(99_999.9000));
    }

    #[tokio::test]
    async fn no_warmup_still_calls_snake_case_warmup_finished() {
        crate::test_python::init();

        let tmp = tempfile::tempdir().unwrap();
        let strategy_path = tmp.path().join("main.py");
        let marker_path = tmp.path().join("warmup_called.txt");
        let marker_literal = serde_json::to_string(marker_path.to_str().unwrap()).unwrap();
        let strategy = format!(
            r#"
from AlgorithmImports import *

class NoWarmupCallback(QCAlgorithm):
    def initialize(self):
        self.set_start_date(2024, 1, 2)
        self.set_end_date(2024, 1, 2)
        self.set_cash(100000)

    def on_warmup_finished(self):
        with open({marker_literal}, "w") as f:
            f.write("called")
"#
        );
        std::fs::write(&strategy_path, strategy).unwrap();

        run_strategy(
            &strategy_path,
            RunConfig {
                data_root: tmp.path().join("data"),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let marker = std::fs::read_to_string(marker_path).unwrap();
        assert_eq!(marker, "called");
    }

    #[test]
    fn default_benchmark_is_spy_when_not_set() {
        let alg = make_alg();
        // benchmark_symbol is None → runner defaults to SPY
        assert!(alg.benchmark_symbol.is_none());
        let effective = alg.benchmark_symbol.unwrap_or_else(|| "SPY".to_string());
        assert_eq!(effective, "SPY");
    }

    #[test]
    fn missing_strategy_end_date_defaults_to_yesterday() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let end_date = resolve_backtest_end_date(None, DateTime::MAX, today);
        assert_eq!(
            end_date,
            chrono::NaiveDate::from_ymd_opt(2026, 5, 3).unwrap()
        );
    }

    #[test]
    fn explicit_end_date_override_wins_over_missing_strategy_end_date() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let override_date = chrono::NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        let end_date = resolve_backtest_end_date(Some(override_date), DateTime::MAX, today);
        assert_eq!(end_date, override_date);
    }

    #[test]
    fn strategy_end_date_wins_when_no_cli_override_is_set() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 5, 4).unwrap();
        let strategy_date = chrono::NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
        let strategy_end = DateTime::from(strategy_date.and_hms_opt(0, 0, 0).unwrap());
        let end_date = resolve_backtest_end_date(None, strategy_end, today);
        assert_eq!(end_date, strategy_date);
    }

    #[test]
    fn set_benchmark_overrides_default() {
        let mut alg = make_alg();
        alg.set_benchmark("QQQ");
        let effective = alg.benchmark_symbol.unwrap_or_else(|| "SPY".to_string());
        assert_eq!(effective, "QQQ");
    }

    #[test]
    fn set_benchmark_uppercases_ticker() {
        let mut alg = make_alg();
        alg.set_benchmark("qqq");
        assert_eq!(alg.benchmark_symbol.as_deref(), Some("QQQ"));
    }

    #[test]
    fn benchmark_returns_computed_from_price_series() {
        // A price series of [100, 110, 99] corresponds to daily returns of
        // [(110-100)/100 = 10%, (99-110)/110 ≈ -10%].
        let prices: Vec<Decimal> = vec![dec!(100), dec!(110), dec!(99)];
        let returns: Vec<Decimal> = prices.windows(2).map(|w| (w[1] - w[0]) / w[0]).collect();

        // 10% up
        let expected_up = dec!(10) / dec!(100);
        assert!((returns[0] - expected_up).abs() < dec!(0.0001));

        // ≈ -10% down
        let expected_down = (dec!(99) - dec!(110)) / dec!(110);
        assert!((returns[1] - expected_down).abs() < dec!(0.0001));
    }

    #[test]
    fn benchmark_symbol_appears_in_backtest_result_field() {
        // Verify the BacktestResult struct carries the benchmark ticker.
        // We build a minimal BacktestResult directly.
        use crate::charting::ChartCollection;
        use lean_statistics::PortfolioStatistics;

        let stats = PortfolioStatistics::compute(
            &[dec!(100_000), dec!(101_000)],
            &[dec!(400), dec!(402)],
            &[],
            1,
            dec!(100_000),
            dec!(0),
        );
        let result = BacktestResult {
            trading_days: 1,
            final_value: 101_000.0,
            total_return: 0.01,
            starting_cash: 100_000.0,
            start_date: chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            end_date: chrono::NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
            equity_curve: vec![100_000.0, 101_000.0],
            daily_dates: vec!["2024-01-02".to_string(), "2024-01-03".to_string()],
            benchmark_curve: vec![400.0, 402.0],
            benchmark_dates: vec!["2024-01-02".to_string(), "2024-01-03".to_string()],
            statistics: stats,
            charts: ChartCollection::default(),
            order_events: vec![],
            succeeded_data_requests: vec![],
            failed_data_requests: vec![],
            backtest_id: 1_700_000_000,
            benchmark_symbol: "QQQ".to_string(),
        };
        assert_eq!(result.benchmark_symbol, "QQQ");
    }

    #[test]
    fn benchmark_symbol_for_spy_equity() {
        // Verify that SPY symbol creation is stable and has the correct SID.
        let market = lean_core::Market::usa();
        let spy_a = Symbol::create_equity("SPY", &market);
        let spy_b = Symbol::create_equity("SPY", &market);
        // Two independently created SPY symbols must have the same SID so the
        // benchmark price map lookup works correctly.
        assert_eq!(spy_a.id.sid, spy_b.id.sid);
        assert_eq!(spy_a.permtick, "SPY");
    }

    #[test]
    fn benchmark_resolver_keeps_provider_market_for_xyz_symbols() {
        let benchmark = resolve_benchmark_symbol("XYZ:SP500", &[]);
        assert_eq!(benchmark.security_type(), SecurityType::CryptoFuture);
        assert_eq!(benchmark.market().as_str(), Market::HYPERLIQUID);
        assert_eq!(benchmark.permtick, "XYZ:SP500");
    }

    #[test]
    fn benchmark_in_subs_detected_correctly() {
        use lean_core::{Market, Resolution, Symbol};
        use lean_data::SubscriptionDataConfig;
        use std::sync::Arc;

        let market = Market::usa();
        let spy = Symbol::create_equity("SPY", &market);

        let cfg_spy = Arc::new(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Daily,
        ));
        let subs = [cfg_spy];

        // SPY is in subs → benchmark_in_subs = true
        let benchmark = resolve_benchmark_symbol("SPY", &subs);
        assert!(benchmark_symbol_in_subscriptions(&benchmark, &subs));

        // QQQ is NOT in subs → benchmark_in_subs = false
        let benchmark2 = resolve_benchmark_symbol("QQQ", &subs);
        assert!(!benchmark_symbol_in_subscriptions(&benchmark2, &subs));
    }

    #[test]
    fn load_trade_bar_partitions_reads_date_partition_range() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let writer = ParquetWriter::new(WriterConfig::default());
        let market = lean_core::Market::usa();
        let symbol = Symbol::create_equity("SPY", &market);
        let sub = SubscriptionDataConfig::new_equity(symbol.clone(), Resolution::Daily);
        let day1 = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let day2 = chrono::NaiveDate::from_ymd_opt(2024, 1, 3).unwrap();

        let make_bar = |date: chrono::NaiveDate, close| {
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(date, 16, 0, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(close, close, close, close, dec!(1000)),
            )
        };

        for (date, close) in [(day1, dec!(100)), (day2, dec!(101))] {
            let path =
                resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, date);
            writer
                .write_trade_bars(&[make_bar(date, close)], &path)
                .unwrap();
        }

        let params = QueryParams::new()
            .with_time_range(
                date_to_datetime(day1, 0, 0, 0),
                date_to_datetime(day2, 23, 59, 59),
            )
            .with_symbols(vec![symbol.id.sid]);
        let rows =
            load_trade_bar_partitions(&ParquetReader::new(), &resolver, &sub, day1, day2, &params);
        let closes: Vec<_> = rows.iter().map(|bar| bar.close).collect();

        assert_eq!(closes, vec![dec!(100), dec!(101)]);
    }

    fn custom_subscription_for_test(
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        role: lean_data::CustomDataSubscriptionRole,
    ) -> CustomDataSubscription {
        let config = CustomDataConfig {
            ticker: ticker.to_string(),
            source_type: source_type.to_string(),
            resolution,
            properties: HashMap::new(),
            query: CustomDataQuery::default(),
        };
        CustomDataSubscription {
            source_type: source_type.to_string(),
            ticker: ticker.to_string(),
            config,
            dynamic_query: CustomDataQuery::default(),
            role,
        }
    }

    #[tokio::test]
    async fn low_resolution_custom_loader_skips_universe_subscriptions() {
        let tmp = tempfile::tempdir().unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let path = custom_data_path(tmp.path(), "fixture", "ALT", date);
        let mut fields = HashMap::new();
        fields.insert("symbol".to_string(), serde_json::json!("ALT"));
        ParquetWriter::new(WriterConfig::default())
            .write_custom_data_points(
                &[CustomDataPoint {
                    time: date,
                    end_time: Some(date_to_datetime(date, 16, 0, 0)),
                    value: dec!(42),
                    fields,
                }],
                &path,
            )
            .unwrap();

        let config = RunConfig {
            data_root: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let sub = custom_subscription_for_test(
            "fixture",
            "ALT",
            Resolution::Daily,
            lean_data::CustomDataSubscriptionRole::Universe,
        );
        let rows = load_low_resolution_custom_data_for_day(&[sub], &HashMap::new(), &config, date)
            .await
            .unwrap();

        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn low_resolution_universe_loader_reads_date_local_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let path = custom_data_path(tmp.path(), "fixture", "ALT", date);
        let mut fields = HashMap::new();
        fields.insert("symbol".to_string(), serde_json::json!("ALT"));
        ParquetWriter::new(WriterConfig::default())
            .write_custom_data_points(
                &[CustomDataPoint {
                    time: date,
                    end_time: Some(date_to_datetime(date, 16, 0, 0)),
                    value: dec!(42),
                    fields,
                }],
                &path,
            )
            .unwrap();

        let config = RunConfig {
            data_root: tmp.path().to_path_buf(),
            ..Default::default()
        };
        let sub = custom_subscription_for_test(
            "fixture",
            "ALT",
            Resolution::Daily,
            lean_data::CustomDataSubscriptionRole::Universe,
        );
        let rows = load_low_resolution_universe_data_for_day(&[sub], &config, date)
            .await
            .unwrap();

        assert_eq!(rows.get("ALT").map(Vec::len), Some(1));
        assert_eq!(rows["ALT"][0].value, dec!(42));
    }

    struct FullHistoryFixtureSource;

    impl lean_data_providers::ICustomDataSource for FullHistoryFixtureSource {
        fn name(&self) -> &str {
            "fixture"
        }

        fn is_full_history_source(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn low_resolution_universe_loader_reads_full_history_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let date = chrono::NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let path = custom_data_history_path(tmp.path(), "fixture", "ALT");
        let mut fields = HashMap::new();
        fields.insert("symbol".to_string(), serde_json::json!("ALT"));
        ParquetWriter::new(WriterConfig::default())
            .write_custom_data_points(
                &[CustomDataPoint {
                    time: date,
                    end_time: Some(date_to_datetime(date, 16, 0, 0)),
                    value: dec!(42),
                    fields,
                }],
                &path,
            )
            .unwrap();

        let config = RunConfig {
            data_root: tmp.path().to_path_buf(),
            custom_data_sources: vec![Arc::new(FullHistoryFixtureSource)],
            ..Default::default()
        };
        let sub = custom_subscription_for_test(
            "fixture",
            "ALT",
            Resolution::Daily,
            lean_data::CustomDataSubscriptionRole::Universe,
        );
        let rows = load_low_resolution_universe_data_for_day(&[sub], &config, date)
            .await
            .unwrap();

        assert_eq!(rows.get("ALT").map(Vec::len), Some(1));
        assert_eq!(rows["ALT"][0].value, dec!(42));
    }

    #[tokio::test]
    async fn benchmark_loader_reads_full_date_partition_range() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let writer = ParquetWriter::new(WriterConfig::default());
        let symbol = Symbol::create_crypto_future("XYZ:SP500", &Market::hyperliquid());
        let day1 = chrono::NaiveDate::from_ymd_opt(2026, 6, 8).unwrap();
        let day2 = chrono::NaiveDate::from_ymd_opt(2026, 6, 9).unwrap();

        let make_bar = |date: chrono::NaiveDate, close| {
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(date, 23, 0, 0),
                TimeSpan::ONE_HOUR,
                TradeBarData::new(close, close, close, close, dec!(1000)),
            )
        };

        for (date, close) in [(day1, dec!(110)), (day2, dec!(112))] {
            let path =
                resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, date);
            writer
                .write_trade_bars(&[make_bar(date, close)], &path)
                .unwrap();
        }

        let prices = load_internal_benchmark_prices_for_resolution(
            None,
            &ParquetReader::new(),
            &resolver,
            &symbol,
            Resolution::Daily,
            day1,
            day2,
        )
        .await;

        assert_eq!(prices.get(&day1), Some(&dec!(110)));
        assert_eq!(prices.get(&day2), Some(&dec!(112)));
    }

    #[test]
    fn benchmark_alignment_drops_leading_missing_dates_and_fill_forwards() {
        let equity = vec![dec!(100), dec!(101), dec!(102), dec!(103), dec!(104)];
        let daily_dates = vec![
            "2026-01-01".to_string(),
            "2026-01-02".to_string(),
            "2026-01-03".to_string(),
            "2026-01-04".to_string(),
            "2026-01-05".to_string(),
        ];
        let benchmark = vec![dec!(200), dec!(204)];
        let benchmark_dates = vec!["2026-01-03".to_string(), "2026-01-05".to_string()];

        let (aligned_equity, aligned_benchmark) = align_benchmark_curve_to_equity_dates(
            &equity,
            &daily_dates,
            &benchmark,
            &benchmark_dates,
        );

        assert_eq!(aligned_equity, vec![dec!(102), dec!(103), dec!(104)]);
        assert_eq!(aligned_benchmark, vec![dec!(200), dec!(200), dec!(204)]);
    }

    #[test]
    fn high_resolution_custom_data_is_bucketed_by_end_time() {
        let event_time = chrono_tz::America::New_York
            .with_ymd_and_hms(2026, 4, 20, 9, 30, 0)
            .unwrap();
        let end_time = chrono_tz::America::New_York
            .with_ymd_and_hms(2026, 4, 20, 9, 31, 0)
            .unwrap();
        let event_time_utc = DateTime::from(event_time.with_timezone(&chrono::Utc));
        let end_time_utc = DateTime::from(end_time.with_timezone(&chrono::Utc));
        let mut fields = HashMap::new();
        fields.insert("time".to_string(), serde_json::json!("2026-04-20 09:30:00"));
        let point = CustomDataPoint {
            time: event_time.date_naive(),
            end_time: Some(end_time_utc),
            value: dec!(1),
            fields,
        };
        let mut buckets = HashMap::new();

        bucket_high_resolution_custom_points_by_end_time(&mut buckets, "flow_alerts", vec![point]);

        assert!(!buckets.contains_key(&event_time_utc.0));
        let bucket = &buckets[&end_time_utc.0]["flow_alerts"];
        assert_eq!(bucket.len(), 1);
        assert_eq!(
            bucket[0].fields.get("time"),
            Some(&serde_json::json!("2026-04-20 09:30:00"))
        );
    }

    #[test]
    fn reconcile_runner_subscriptions_prunes_removed_and_tracks_new_configs() {
        let market = Market::usa();
        let spy = Symbol::create_equity("SPY", &market);
        let qqq = Symbol::create_equity("QQQ", &market);
        let aapl = Symbol::create_equity("AAPL", &market);

        let spy_trade = Arc::new(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
        ));
        let mut spy_quote_cfg = SubscriptionDataConfig::new_equity(spy, Resolution::Minute);
        spy_quote_cfg.tick_type = TickType::Quote;
        let spy_quote = Arc::new(spy_quote_cfg);
        let qqq_trade = Arc::new(SubscriptionDataConfig::new_equity(qqq, Resolution::Minute));
        let aapl_trade = Arc::new(SubscriptionDataConfig::new_equity(aapl, Resolution::Minute));

        let mut subscriptions = vec![spy_trade.clone(), spy_quote.clone(), qqq_trade.clone()];
        let mut loaded_subscription_ids: HashSet<u64> =
            subscriptions.iter().map(|sub| sub.unique_id()).collect();

        let first = reconcile_runner_subscriptions(
            &mut subscriptions,
            &mut loaded_subscription_ids,
            std::slice::from_ref(&spy_trade),
        );
        assert!(first.new_subs.is_empty());
        assert_eq!(first.removed_subs.len(), 2);
        assert_eq!(subscriptions.len(), 1);
        assert!(subscriptions
            .iter()
            .any(|sub| sub.unique_id() == spy_trade.unique_id()));
        assert_eq!(loaded_subscription_ids.len(), 1);
        assert!(loaded_subscription_ids.contains(&spy_trade.unique_id()));
        assert!(!loaded_subscription_ids.contains(&spy_quote.unique_id()));
        assert!(!loaded_subscription_ids.contains(&qqq_trade.unique_id()));

        let second = reconcile_runner_subscriptions(
            &mut subscriptions,
            &mut loaded_subscription_ids,
            &[spy_trade.clone(), aapl_trade.clone()],
        );
        assert_eq!(second.new_subs.len(), 1);
        assert_eq!(second.new_subs[0].unique_id(), aapl_trade.unique_id());
        assert_eq!(subscriptions.len(), 2);
        assert!(loaded_subscription_ids.contains(&spy_trade.unique_id()));
        assert!(loaded_subscription_ids.contains(&aapl_trade.unique_id()));
    }

    #[test]
    fn build_option_chain_from_eod_bars_populates_model_data() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let valuation_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 20, 0, 0)
                .single()
                .unwrap(),
        );

        let bars = vec![OptionEodBar {
            date: valuation_time.date_utc(),
            symbol_value: "SPY240119C00100000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(100),
            right: "C".to_string(),
            open: dec!(2.50),
            high: dec!(2.60),
            low: dec!(2.40),
            close: dec!(2.50),
            volume: 42,
            bid: dec!(2.40),
            ask: dec!(2.60),
            bid_size: 10,
            ask_size: 12,
        }];

        let chain = build_option_chain_from_eod_bars(
            &canonical,
            dec!(100),
            valuation_time,
            &bars,
            None,
            &[],
        );
        let contract = chain.contracts.values().next().unwrap();

        assert!(contract.data.implied_volatility > Decimal::ZERO);
        assert!(contract.data.theoretical_price > Decimal::ZERO);
        assert!(contract.data.greeks.delta > Decimal::ZERO);
    }

    #[test]
    fn option_universe_filter_retains_held_contracts() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let date = chrono::NaiveDate::from_ymd_opt(2024, 1, 18).unwrap();
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let held = Symbol::create_option_osi(
            underlying.clone(),
            dec!(110),
            expiry,
            OptionRight::Put,
            OptionStyle::American,
            &market,
        );
        let mut rows = vec![
            OptionUniverseRow {
                date,
                symbol_value: "SPY240119C00100000".to_string(),
                underlying: "SPY".to_string(),
                expiration: expiry,
                strike: dec!(100),
                right: "C".to_string(),
            },
            OptionUniverseRow {
                date,
                symbol_value: held.value.clone(),
                underlying: "SPY".to_string(),
                expiration: expiry,
                strike: dec!(110),
                right: "P".to_string(),
            },
        ];
        let filter = OptionFilter {
            min_strike_rank: 0,
            max_strike_rank: 0,
            min_expiry_days: 0,
            max_expiry_days: 5,
        };

        apply_option_universe_filter_to_rows(&mut rows, date, dec!(100), Some(filter));
        assert!(!rows
            .iter()
            .filter_map(option_universe_row_symbol)
            .any(|symbol| symbol.id.sid == held.id.sid));

        append_option_universe_rows_for_contracts(&mut rows, std::slice::from_ref(&held), date);
        let chain = build_option_chain_from_universe_rows(&canonical, dec!(100), &rows);

        assert!(chain
            .contracts
            .keys()
            .any(|symbol| symbol.id.sid == held.id.sid));
    }

    #[test]
    fn eod_option_chain_filter_retains_held_contracts() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let valuation_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 20, 0, 0)
                .single()
                .unwrap(),
        );
        let held = Symbol::create_option_osi(
            underlying.clone(),
            dec!(110),
            expiry,
            OptionRight::Put,
            OptionStyle::American,
            &market,
        );
        let bars = vec![
            OptionEodBar {
                date: valuation_time.date_utc(),
                symbol_value: "SPY240119C00100000".to_string(),
                underlying: "SPY".to_string(),
                expiration: expiry,
                strike: dec!(100),
                right: "C".to_string(),
                open: dec!(2.50),
                high: dec!(2.60),
                low: dec!(2.40),
                close: dec!(2.50),
                volume: 42,
                bid: dec!(2.40),
                ask: dec!(2.60),
                bid_size: 10,
                ask_size: 12,
            },
            OptionEodBar {
                date: valuation_time.date_utc(),
                symbol_value: held.value.clone(),
                underlying: "SPY".to_string(),
                expiration: expiry,
                strike: dec!(110),
                right: "P".to_string(),
                open: dec!(11.00),
                high: dec!(11.20),
                low: dec!(10.80),
                close: dec!(11.00),
                volume: 7,
                bid: dec!(10.90),
                ask: dec!(11.10),
                bid_size: 4,
                ask_size: 5,
            },
        ];
        let filter = OptionFilter {
            min_strike_rank: 0,
            max_strike_rank: 0,
            min_expiry_days: 0,
            max_expiry_days: 5,
        };

        let chain = build_option_chain_from_eod_bars(
            &canonical,
            dec!(100),
            valuation_time,
            &bars,
            Some(filter),
            std::slice::from_ref(&held),
        );

        assert!(chain
            .contracts
            .keys()
            .any(|symbol| symbol.id.sid == held.id.sid));
    }

    #[test]
    fn option_chain_runtime_reprices_from_latest_quote_and_current_underlying() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();

        let rows = vec![OptionUniverseRow {
            date: chrono::NaiveDate::from_ymd_opt(2024, 1, 18).unwrap(),
            symbol_value: "SPY240119C00100000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(100),
            right: "C".to_string(),
        }];
        let chain = build_option_chain_from_universe_rows(&canonical, dec!(100), &rows);
        let contract_symbol = chain.contracts.keys().next().unwrap().clone();

        let first_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 15, 0, 0)
                .single()
                .unwrap(),
        );
        let second_time = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 15, 1, 0)
                .single()
                .unwrap(),
        );

        let quote_bar = QuoteBar::new(
            contract_symbol.clone(),
            first_time,
            TimeSpan::ONE_MINUTE,
            Some(Bar::from_price(dec!(2.40))),
            Some(Bar::from_price(dec!(2.60))),
            dec!(10),
            dec!(12),
        );

        let mut runtime = OptionChainRuntime {
            permtick: canonical.permtick.clone(),
            chain,
            trade_updates: HashMap::new(),
            quote_updates: HashMap::from([(first_time.0, vec![quote_bar])]),
            tick_updates: HashMap::new(),
            tick_stream: None,
            pending_tick: None,
            priced_contracts: HashSet::new(),
        };

        runtime.apply_timestamp(first_time, dec!(100), &[]);
        let initial_delta = runtime
            .chain
            .contracts
            .get(&contract_symbol)
            .unwrap()
            .data
            .greeks
            .delta;

        runtime.apply_timestamp(second_time, dec!(105), &[]);
        let repriced = runtime.chain.contracts.get(&contract_symbol).unwrap();

        assert!(repriced.data.implied_volatility > Decimal::ZERO);
        assert!(repriced.data.theoretical_price > Decimal::ZERO);
        assert!(repriced.data.greeks.delta > initial_delta);
    }

    #[test]
    fn option_chain_runtime_does_not_publish_zero_underlying_chain() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();

        let rows = vec![OptionUniverseRow {
            date: chrono::NaiveDate::from_ymd_opt(2024, 1, 18).unwrap(),
            symbol_value: "SPY240119C00100000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(100),
            right: "C".to_string(),
        }];
        let chain = build_option_chain_from_universe_rows(&canonical, Decimal::ZERO, &rows);
        let contract_symbol = chain.contracts.keys().next().unwrap().clone();
        let timestamp = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 15, 0, 0)
                .single()
                .unwrap(),
        );
        let quote_bar = QuoteBar::new(
            contract_symbol.clone(),
            timestamp,
            TimeSpan::ONE_MINUTE,
            Some(Bar::from_price(dec!(2.40))),
            Some(Bar::from_price(dec!(2.60))),
            dec!(10),
            dec!(12),
        );
        let mut runtime = OptionChainRuntime {
            permtick: canonical.permtick.clone(),
            chain,
            trade_updates: HashMap::new(),
            quote_updates: HashMap::from([(timestamp.0, vec![quote_bar])]),
            tick_updates: HashMap::new(),
            tick_stream: None,
            pending_tick: None,
            priced_contracts: HashSet::new(),
        };

        assert!(!runtime.apply_timestamp(timestamp, Decimal::ZERO, &[]));
        assert_eq!(runtime.chain.underlying_price, Decimal::ZERO);
        assert_eq!(
            runtime
                .chain
                .contracts
                .get(&contract_symbol)
                .unwrap()
                .data
                .underlying_last_price,
            Decimal::ZERO
        );
    }

    #[test]
    fn option_chain_runtime_preserves_last_underlying_when_current_spot_missing() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let canonical = Symbol::create_canonical_option(&underlying, &market);
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();

        let rows = vec![OptionUniverseRow {
            date: chrono::NaiveDate::from_ymd_opt(2024, 1, 18).unwrap(),
            symbol_value: "SPY240119C00100000".to_string(),
            underlying: "SPY".to_string(),
            expiration: expiry,
            strike: dec!(100),
            right: "C".to_string(),
        }];
        let chain = build_option_chain_from_universe_rows(&canonical, dec!(100), &rows);
        let contract_symbol = chain.contracts.keys().next().unwrap().clone();
        let timestamp = DateTime::from(
            chrono::Utc
                .with_ymd_and_hms(2024, 1, 18, 16, 0, 0)
                .single()
                .unwrap(),
        );
        let quote_bar = QuoteBar::new(
            contract_symbol.clone(),
            timestamp,
            TimeSpan::ONE_MINUTE,
            Some(Bar::from_price(dec!(2.40))),
            Some(Bar::from_price(dec!(2.60))),
            dec!(10),
            dec!(12),
        );
        let mut runtime = OptionChainRuntime {
            permtick: canonical.permtick.clone(),
            chain,
            trade_updates: HashMap::new(),
            quote_updates: HashMap::from([(timestamp.0, vec![quote_bar])]),
            tick_updates: HashMap::new(),
            tick_stream: None,
            pending_tick: None,
            priced_contracts: HashSet::new(),
        };

        assert!(runtime.apply_timestamp(timestamp, Decimal::ZERO, &[]));
        assert_eq!(runtime.chain.underlying_price, dec!(100));
        assert_eq!(
            runtime
                .chain
                .contracts
                .get(&contract_symbol)
                .unwrap()
                .data
                .underlying_last_price,
            dec!(100)
        );
    }
}
