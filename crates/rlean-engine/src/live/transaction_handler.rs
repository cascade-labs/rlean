//! Engine-owned live transaction handler.
//!
//! Modeled on C# LEAN's `BrokerageTransactionHandler`. With a sidecar execution
//! connection, new orders are sent over the persistent Flight exchange and
//! fills/status changes are pushed back by the sidecar instead of being filled
//! locally by the `ImmediateFillModel`.
//!
//! A dedicated worker owns the sidecar event stream. The live loop communicates
//! with it over two crossbeam channels:
//!
//!   * request channel (loop -> worker): submit/cancel/update/cash-sync/stop.
//!   * event channel (worker -> loop): status + fill notifications.
//!
//! Portfolio, `TransactionManager` and algorithm callbacks are only ever touched
//! on the live-loop thread. The worker only talks to the brokerage.

use anyhow::Context;
use chrono::Timelike;
use crossbeam_channel::{Receiver, Sender};
use rlean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices};
use rlean_algorithm::portfolio::SecurityPortfolioManager;
use rlean_algorithm::qc_algorithm::{AccountType, QcAlgorithm};
#[cfg(test)]
use rlean_brokerages::Brokerage;
use rlean_core::{DateTime, Price, Quantity};
use rlean_data_sidecar::{
    order_status_from_wire, symbol_from_wire, BrokerageEvent as SidecarBrokerageEvent,
};
use rlean_orders::{Order, OrderEvent, OrderStatus, OrderType, TransactionManager};
use rlean_statistics::{Trade, TradeBuilder};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;

use crate::algorithm_manager::AlgorithmManager;
use crate::orders::record_trade_fill;

/// A request sent from the live loop to the brokerage worker thread.
enum BrokerageRequest {
    /// Submit a brand-new order to the brokerage.
    Submit(Order),
    /// Cancel an order that has a brokerage id.
    Cancel(Order),
    /// Update an order that has a brokerage id.
    Update(Order),
    /// Pull an authoritative cash snapshot from the brokerage.
    CashSync,
    /// Shut the worker down.
    Stop,
}

/// A brokerage-originated notification for the live loop to apply.
///
/// `pub(crate)` so engine-side tests (e.g. the runner's cross-symbol guard
/// regression) can construct events and drive `apply_event` directly.
#[derive(Debug, Clone)]
pub(crate) enum BrokerageEvent {
    /// The order was accepted by the brokerage; record its brokerage ids and
    /// transition New -> Submitted.
    Submitted {
        order_id: i64,
        brokerage_ids: Vec<String>,
    },
    /// Submission was rejected by the brokerage.
    Invalid { order_id: i64, message: String },
    /// Submission failed with a transport/transient error. Distinct from
    /// `Invalid`: a broker rejection is terminal, while this failure gets a
    /// bounded number of retries before the order is finally marked `Invalid`.
    SubmitFailed { order_id: i64, message: String },
    /// A pushed status/fill update. `cumulative_filled` is the
    /// signed total filled quantity the brokerage reports for the order. `symbol`
    /// is the symbol the brokerage reports; the loop verifies
    /// it matches the engine order's symbol before applying any effect, so a
    /// mis-resolved id can never apply a fill across symbols (issue #33).
    Status {
        order_id: i64,
        symbol: rlean_core::Symbol,
        status: OrderStatus,
        fill_price: Price,
        cumulative_filled: Quantity,
        commission: Option<Price>,
    },
    /// Authoritative per-currency cash balances returned by the brokerage.
    CashSnapshot { cash_balances: Vec<(String, Price)> },
    /// A cash snapshot failed. Trading continues and the synchronization state
    /// retries with backoff; a brokerage outage must not terminate a strategy.
    CashSyncFailed { message: String },
}

/// Backoff schedule for retrying a failed order submission. Its length + 1 is
/// the total number of submission attempts: with two delays the order is tried
/// up to three times (initial submit, then a retry 5s later, then 15s later).
/// A submission that still fails after the last delay goes terminally `Invalid`.
///
/// The delays are deliberately short and few: a retryable sidecar response is
/// most often a transient transport hiccup (e.g. a response-decode failure), so a
/// couple of spaced-out retries recover it, while a genuine rejection surfaced
/// as `Err` just fails a couple more times and reaches the same terminal state
/// a few seconds later.
const SUBMIT_BACKOFF: [std::time::Duration; 2] = [
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(15),
];

const CASH_SYNC_HOUR_ET: u32 = 7;
const CASH_SYNC_MINUTE_ET: u32 = 45;
const CASH_SYNC_MIN_FILL_AGE_NS: i64 = 10_000_000_000;
const CASH_SYNC_VERIFY_DELAY_NS: i64 = 10_000_000_000;
const CASH_SYNC_RECENT_FILL_NS: i64 = 20_000_000_000;
const CASH_SYNC_MAX_RETRY_NS: i64 = 60_000_000_000;

#[derive(Debug, Clone)]
struct LiveCashSyncState {
    last_sync_date_et: chrono::NaiveDate,
    request_in_flight: bool,
    retry_not_before: Option<DateTime>,
    failures: u32,
    verify_at: Option<DateTime>,
    last_fill_time: Option<DateTime>,
}

impl LiveCashSyncState {
    fn new(now: DateTime) -> Self {
        Self {
            // Startup account synchronization is authoritative for the current
            // day, matching C# LEAN's `_syncedLiveBrokerageCashToday = true`.
            last_sync_date_et: now.to_tz(rlean_core::time::tz::NEW_YORK).date_naive(),
            request_in_flight: false,
            retry_not_before: None,
            failures: 0,
            verify_at: None,
            last_fill_time: None,
        }
    }

    fn record_fill(&mut self, now: DateTime) {
        self.last_fill_time = Some(now);
    }

    fn should_request(&mut self, now: DateTime) -> bool {
        if self.verify_at.is_some_and(|verify_at| now >= verify_at) {
            self.verify_at = None;
            if self
                .last_fill_time
                .is_some_and(|fill| now.0.saturating_sub(fill.0) <= CASH_SYNC_RECENT_FILL_NS)
            {
                // The snapshot may have raced a fill. Make the current day
                // eligible again so the next quiet interval re-synchronizes.
                self.last_sync_date_et = self
                    .last_sync_date_et
                    .pred_opt()
                    .unwrap_or(self.last_sync_date_et);
                tracing::info!(
                    "brokerage cash sync was followed by a recent fill; resynchronization required"
                );
            } else {
                tracing::info!("brokerage cash sync verified");
            }
        }

        if self.request_in_flight
            || self
                .retry_not_before
                .is_some_and(|retry_not_before| now < retry_not_before)
            || self
                .last_fill_time
                .is_some_and(|fill| now.0.saturating_sub(fill.0) <= CASH_SYNC_MIN_FILL_AGE_NS)
        {
            return false;
        }

        let now_et = now.to_tz(rlean_core::time::tz::NEW_YORK);
        let after_sync_time = now_et.hour() > CASH_SYNC_HOUR_ET
            || (now_et.hour() == CASH_SYNC_HOUR_ET && now_et.minute() >= CASH_SYNC_MINUTE_ET);
        after_sync_time && now_et.date_naive() != self.last_sync_date_et
    }

    fn request_started(&mut self) {
        self.request_in_flight = true;
    }

    fn request_succeeded(&mut self, now: DateTime) {
        self.last_sync_date_et = now.to_tz(rlean_core::time::tz::NEW_YORK).date_naive();
        self.request_in_flight = false;
        self.retry_not_before = None;
        self.failures = 0;
        self.verify_at = Some(rlean_core::NanosecondTimestamp(
            now.0.saturating_add(CASH_SYNC_VERIFY_DELAY_NS),
        ));
    }

    fn request_failed(&mut self, now: DateTime) {
        self.request_in_flight = false;
        self.failures = self.failures.saturating_add(1);
        let exponent = self.failures.saturating_sub(1).min(5);
        let delay_ns = 2_i64
            .saturating_pow(exponent)
            .saturating_mul(1_000_000_000)
            .min(CASH_SYNC_MAX_RETRY_NS);
        self.retry_not_before = Some(rlean_core::NanosecondTimestamp(
            now.0.saturating_add(delay_ns),
        ));
    }
}

/// State the worker tracks per order it has seen from the brokerage, so it only
/// emits an event when something actually changed.
#[cfg(test)]
#[derive(Default, Clone)]
struct TrackedBrokerageOrder {
    status: Option<OrderStatus>,
    filled: Quantity,
}

/// Live transaction handler that routes orders to a real brokerage.
///
/// Constructed only when `Brokerage` is `Some` and `!uses_local_paper_fills()`.
pub struct LiveBrokerageRouter {
    request_tx: Sender<BrokerageRequest>,
    event_rx: Receiver<BrokerageEvent>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// Order ids the loop has already forwarded to the worker for submission, so
    /// a `New` order is not submitted twice while it is in-flight.
    submitted_ids: std::collections::HashSet<i64>,
    /// Cancel/update requests already forwarded, to avoid re-forwarding each tick.
    cancel_forwarded: std::collections::HashSet<i64>,
    update_forwarded: std::collections::HashSet<i64>,
    /// Cumulative filled quantity already applied to the portfolio per order, so
    /// repeated pushed snapshots only apply the incremental delta.
    applied_filled: HashMap<i64, Quantity>,
    /// Cumulative brokerage fees already applied per order. Sidecar updates are
    /// snapshots and may be repeated, so only the incremental fee is charged.
    applied_fees: HashMap<i64, Price>,
    /// Backoff schedule between submission retries. Kept as a field (rather than a
    /// bare const) so tests can inject a fast schedule without sleeping seconds.
    /// `submit_backoff.len() + 1` is the total number of submission attempts.
    submit_backoff: Vec<std::time::Duration>,
    /// Number of submission attempts already dispatched per order id, so a failed
    /// submit is retried only up to `submit_backoff.len() + 1` times.
    submit_attempts: HashMap<i64, u32>,
    /// Earliest instant a failed order may be resubmitted. `dispatch_pending`
    /// skips a `New` order whose entry here is still in the future, so the retry
    /// wait never blocks the worker or busy-waits.
    retry_not_before: HashMap<i64, std::time::Instant>,
    /// Market-on-open orders already logged as deferred, so holding an order
    /// across a closed market logs once instead of once per live iteration.
    moo_deferred_logged: std::collections::HashSet<i64>,
    /// Position-reducing orders waiting for their security's first usable
    /// market price. LEAN seeds brokerage holdings with GetLastKnownPrice before
    /// trading; this is the asynchronous safety net when both the brokerage
    /// snapshot and history seed are temporarily empty.
    price_deferred_logged: std::collections::HashSet<i64>,
    /// LEAN-style daily authoritative brokerage cash reconciliation.
    cash_sync: LiveCashSyncState,
}

fn order_reduces_position(algorithm: &QcAlgorithm, order: &Order) -> bool {
    let holdings = algorithm.portfolio.get_holding(&order.symbol).quantity;
    let remaining = order.remaining_quantity();
    holdings != Decimal::ZERO
        && ((holdings > Decimal::ZERO && remaining < Decimal::ZERO)
            || (holdings < Decimal::ZERO && remaining > Decimal::ZERO))
        && (holdings + remaining).abs() < holdings.abs()
}

fn fee_model_order_for_fill(order: &Order, fill_quantity: Quantity) -> Order {
    let mut fill_order = order.clone();
    fill_order.quantity = fill_quantity;
    fill_order.filled_quantity = Decimal::ZERO;
    fill_order
}

impl LiveBrokerageRouter {
    /// Spawn the worker thread and hand ownership of the brokerage to it, using
    /// the default `SUBMIT_BACKOFF` retry schedule.
    #[cfg(test)]
    pub fn spawn(brokerage: Box<dyn Brokerage>) -> Self {
        Self::spawn_with_backoff(brokerage, SUBMIT_BACKOFF.to_vec())
    }

    /// Spawn with an explicit submission-retry backoff schedule. Tests use this
    /// to inject a fast (near-zero) schedule so retry behavior can be exercised
    /// without sleeping the production 5s/15s delays.
    #[cfg(test)]
    pub fn spawn_with_backoff(
        brokerage: Box<dyn Brokerage>,
        submit_backoff: Vec<std::time::Duration>,
    ) -> Self {
        let (request_tx, request_rx) = crossbeam_channel::unbounded::<BrokerageRequest>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded::<BrokerageEvent>();
        let worker = std::thread::Builder::new()
            .name("live-brokerage-router".to_string())
            .spawn(move || run_worker(brokerage, request_rx, event_tx))
            .expect("failed to spawn live brokerage router worker");
        Self {
            request_tx,
            event_rx,
            worker: Some(worker),
            submitted_ids: std::collections::HashSet::new(),
            cancel_forwarded: std::collections::HashSet::new(),
            update_forwarded: std::collections::HashSet::new(),
            applied_filled: HashMap::new(),
            applied_fees: HashMap::new(),
            submit_backoff,
            submit_attempts: HashMap::new(),
            retry_not_before: HashMap::new(),
            moo_deferred_logged: std::collections::HashSet::new(),
            price_deferred_logged: std::collections::HashSet::new(),
            cash_sync: LiveCashSyncState::new(DateTime::now()),
        }
    }

    /// Route live orders through the authenticated sidecar execution
    /// connection. The worker consumes pushed brokerage events; it never polls
    /// the remote brokerage from rlean.
    pub fn spawn_sidecar(connection: crate::runner_config::SidecarBrokerageConnection) -> Self {
        let (request_tx, request_rx) = crossbeam_channel::unbounded::<BrokerageRequest>();
        let (event_tx, event_rx) = crossbeam_channel::unbounded::<BrokerageEvent>();
        let worker = std::thread::Builder::new()
            .name("live-sidecar-brokerage-router".to_string())
            .spawn(move || run_sidecar_worker(connection, request_rx, event_tx))
            .expect("failed to spawn live sidecar brokerage router worker");
        Self {
            request_tx,
            event_rx,
            worker: Some(worker),
            submitted_ids: std::collections::HashSet::new(),
            cancel_forwarded: std::collections::HashSet::new(),
            update_forwarded: std::collections::HashSet::new(),
            applied_filled: HashMap::new(),
            applied_fees: HashMap::new(),
            submit_backoff: SUBMIT_BACKOFF.to_vec(),
            submit_attempts: HashMap::new(),
            retry_not_before: HashMap::new(),
            moo_deferred_logged: std::collections::HashSet::new(),
            price_deferred_logged: std::collections::HashSet::new(),
            cash_sync: LiveCashSyncState::new(DateTime::now()),
        }
    }

    /// Request C# LEAN-style authoritative cash synchronization when the
    /// brokerage session has crossed into a new New York date and it is at
    /// least 07:45 ET. The request is non-blocking and runs through the worker
    /// that owns the current sidecar connection id, so reconnects cannot leave
    /// this path pointing at a stale connection.
    pub fn request_cash_sync_if_due(&mut self, now: DateTime) {
        if !self.cash_sync.should_request(now) {
            return;
        }
        match self.request_tx.send(BrokerageRequest::CashSync) {
            Ok(()) => {
                self.cash_sync.request_started();
                tracing::info!("requesting authoritative brokerage cash synchronization");
            }
            Err(error) => {
                self.cash_sync.request_failed(now);
                tracing::warn!("could not queue brokerage cash synchronization: {error}");
            }
        }
    }

    /// Forward any `New` orders in the transaction manager to the brokerage, and
    /// forward pending cancel/update requests. Called once per live iteration on
    /// the loop thread; never blocks on the brokerage. Evaluates market hours at
    /// the current wall-clock time.
    pub fn dispatch_pending(
        &mut self,
        transactions: &Arc<TransactionManager>,
        algorithm: &QcAlgorithm,
    ) -> Vec<OrderEvent> {
        self.dispatch_pending_at(transactions, algorithm, DateTime::now())
    }

    /// `dispatch_pending` with an explicit market-hours evaluation time, so tests
    /// can drive closed-market (weekend) and open-market dispatch deterministically.
    pub fn dispatch_pending_at(
        &mut self,
        transactions: &Arc<TransactionManager>,
        algorithm: &QcAlgorithm,
        now: DateTime,
    ) -> Vec<OrderEvent> {
        let mut invalid_events = Vec::new();
        let now_instant = std::time::Instant::now();
        let mut open_orders = transactions.get_open_orders();
        let cash_account = algorithm.brokerage_model.account_type == AccountType::Cash;
        let has_pending_reduction = cash_account
            && open_orders
                .iter()
                .any(|order| order_reduces_position(algorithm, order));

        // C# LEAN's target ordering submits position reductions before orders
        // that consume buying power. Preserve that ordering across the live
        // brokerage boundary as well: TransactionManager storage order is not
        // an execution-ordering contract.
        open_orders.sort_by_key(|order| {
            (
                !order_reduces_position(algorithm, order),
                order.created_time,
                order.id,
            )
        });

        for order in open_orders {
            // A cash account cannot spend anticipated sale proceeds. Keep
            // expansion orders New while a position reduction is working; the
            // live loop services brokerage events independently of data slices
            // and dispatches these orders immediately after the fill updates
            // cash/holdings. This is the event-driven equivalent of C# LEAN's
            // synchronous sell-before-buy MarketOrder sequence.
            if has_pending_reduction && !order_reduces_position(algorithm, &order) {
                tracing::debug!(
                    order_id = order.id,
                    symbol = %order.symbol.value,
                    "cash account expansion deferred until position reductions close"
                );
                continue;
            }
            match order.status {
                // A `New` order that is not already in flight is (re)submitted
                // once its retry backoff (if any) has elapsed. A first submit has
                // no `retry_not_before` entry, so it dispatches immediately; a
                // failed submit sets one and this skips it until it is due.
                OrderStatus::New
                    if !self.submitted_ids.contains(&order.id)
                        && self
                            .retry_not_before
                            .get(&order.id)
                            .is_none_or(|not_before| now_instant >= *not_before) =>
                {
                    // C# LEAN's BrokerageSetupHandler resolves a zero brokerage
                    // holding price through GetLastKnownPrice before orders can
                    // be submitted. Live sidecar pricing is asynchronous, so if
                    // both snapshot and history seeding are empty, keep a
                    // reducing order New until its first quote instead of
                    // terminally invalidating a liquidation that requires no
                    // additional buying power.
                    let current_price = algorithm
                        .securities
                        .get(&order.symbol)
                        .map(|security| security.current_price())
                        .unwrap_or_default();
                    if order_reduces_position(algorithm, &order) && current_price <= Decimal::ZERO {
                        if self.price_deferred_logged.insert(order.id) {
                            tracing::warn!(
                                order_id = order.id,
                                symbol = %order.symbol.value,
                                "position-reducing order has no market price; deferring until brokerage, history, or live data seeds the security"
                            );
                        }
                        continue;
                    }
                    self.price_deferred_logged.remove(&order.id);
                    match algorithm.validate_order_submission_buying_power(&order) {
                        Ok(()) => self.dispatch_new_order(order, transactions, now),
                        Err(message) => {
                            tracing::error!(
                                order_id = order.id,
                                symbol = %order.symbol.value,
                                "live order rejected before brokerage submission: {message}"
                            );
                            let mut event =
                                OrderEvent::invalid(order.id, order.symbol.clone(), now, message);
                            event.apply_order_fields(&order);
                            transactions.process_order_event(event.clone());
                            invalid_events.push(event);
                        }
                    }
                }
                OrderStatus::CancelPending
                    if !order.brokerage_id.is_empty() && self.cancel_forwarded.insert(order.id) =>
                {
                    let _ = self.request_tx.send(BrokerageRequest::Cancel(order));
                }
                OrderStatus::UpdateSubmitted
                    if !order.brokerage_id.is_empty() && self.update_forwarded.insert(order.id) =>
                {
                    let _ = self.request_tx.send(BrokerageRequest::Update(order));
                }
                _ => {}
            }
        }
        invalid_events
    }

    /// Dispatch a single `New` order whose backoff (if any) has elapsed,
    /// respecting market hours:
    ///
    /// * A `MarketOnOpen` order is engine-held while its exchange is closed and
    ///   released only once the exchange is open. Sidecar adapters have no
    ///   native market-on-open type, so the released submission goes out as a
    ///   plain market order — the engine-side hold is what provides LEAN's
    ///   "fill at the next market open" semantics.
    /// * A retried order (a previous submit failed) whose exchange has closed
    ///   between attempts is converted to a held `MarketOnOpen` order instead of
    ///   being retried into a closed market. Futures and future options are
    ///   exempt (they trade extended hours), mirroring C# LEAN
    ///   `QCAlgorithm.Trading.cs` `MarketOrder` and
    ///   `DefaultBrokerageModel.CanSubmitOrder`.
    fn dispatch_new_order(
        &mut self,
        order: Order,
        transactions: &Arc<TransactionManager>,
        now: DateTime,
    ) {
        let order_id = order.id;
        let moo_exempt = matches!(
            order.symbol.security_type(),
            rlean_core::SecurityType::Future | rlean_core::SecurityType::FutureOption
        );
        let exchange_open = rlean_core::MarketHoursDatabase::global()
            .exchange_hours(&order.symbol)
            .is_open_at(now);

        if order.order_type == OrderType::MarketOnOpen {
            if !exchange_open {
                if self.moo_deferred_logged.insert(order.id) {
                    tracing::info!(
                        "order {} ({}) market closed; holding as market-on-open until next open",
                        order.id,
                        order.symbol.value,
                    );
                }
                return;
            }
            self.moo_deferred_logged.remove(&order.id);
            tracing::info!(
                "order {} ({}) market open; releasing held market-on-open order",
                order.id,
                order.symbol.value,
            );
        } else if !exchange_open
            && !moo_exempt
            && self.submit_attempts.get(&order.id).copied().unwrap_or(0) > 0
        {
            // The market closed between submission attempts: do not retry into a
            // closed market. Convert to a held market-on-open order; the release
            // above submits it at the next open.
            tracing::info!(
                "order {} ({}) market closed before retry; converting to market-on-open \
                 and holding until next open",
                order.id,
                order.symbol.value,
            );
            let mut held = order;
            held.order_type = OrderType::MarketOnOpen;
            transactions.add_or_update_order(held);
            self.submit_attempts.remove(&order_id);
            self.retry_not_before.remove(&order_id);
            return;
        }

        self.retry_not_before.remove(&order_id);
        self.submitted_ids.insert(order_id);
        *self.submit_attempts.entry(order_id).or_default() += 1;
        // Sidecar brokerage adapters translate market/limit/stop types, so a released
        // market-on-open order crosses the wire as a market order; the engine
        // order keeps its MarketOnOpen type for the audit trail.
        let mut wire_order = order;
        if wire_order.order_type == OrderType::MarketOnOpen {
            wire_order.order_type = OrderType::Market;
        }
        let _ = self.request_tx.send(BrokerageRequest::Submit(wire_order));
    }

    /// Drain brokerage events and apply their portfolio/algorithm effects.
    ///
    /// This is the only place that mutates portfolio/transactions/algorithm from
    /// brokerage input, and it runs on the live-loop thread. Returns the emitted
    /// `OrderEvent`s (already appended to `all_order_events`).
    #[allow(clippy::too_many_arguments)]
    pub fn drain_events<B: AlgorithmBridge>(
        &mut self,
        algorithm_manager: &mut AlgorithmManager<B>,
        services: &mut dyn AlgorithmServices,
        transactions: &Arc<TransactionManager>,
        portfolio: Option<&Arc<SecurityPortfolioManager>>,
        all_order_events: &mut Vec<OrderEvent>,
        trade_builder: &mut TradeBuilder,
        completed_trades: &mut Vec<Trade>,
    ) {
        while let Ok(event) = self.event_rx.try_recv() {
            self.apply_event(
                event,
                algorithm_manager,
                services,
                transactions,
                portfolio,
                all_order_events,
                trade_builder,
                completed_trades,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply_event<B: AlgorithmBridge>(
        &mut self,
        event: BrokerageEvent,
        algorithm_manager: &mut AlgorithmManager<B>,
        services: &mut dyn AlgorithmServices,
        transactions: &Arc<TransactionManager>,
        portfolio: Option<&Arc<SecurityPortfolioManager>>,
        all_order_events: &mut Vec<OrderEvent>,
        trade_builder: &mut TradeBuilder,
        completed_trades: &mut Vec<Trade>,
    ) {
        match event {
            BrokerageEvent::Submitted {
                order_id,
                brokerage_ids,
            } => {
                let Some(mut order) = transactions.get_order(order_id) else {
                    return;
                };
                if order.status != OrderStatus::New {
                    return;
                }
                order.brokerage_id = brokerage_ids;
                order.status = OrderStatus::Submitted;
                transactions.add_or_update_order(order.clone());
                // The order left the submission-retry state machine; drop its
                // per-order retry/hold bookkeeping.
                self.submit_attempts.remove(&order_id);
                self.retry_not_before.remove(&order_id);
                self.moo_deferred_logged.remove(&order_id);

                let mut order_event = OrderEvent::new(
                    order.id,
                    order.symbol.clone(),
                    DateTime::now(),
                    OrderStatus::Submitted,
                );
                order_event.apply_order_fields(&order);
                order_event.message = "Brokerage order submitted".to_string();
                self.emit(
                    order_event,
                    algorithm_manager,
                    services,
                    transactions,
                    all_order_events,
                );
            }
            BrokerageEvent::Invalid { order_id, message } => {
                let Some(mut order) = transactions.get_order(order_id) else {
                    return;
                };
                // Loud in live.log: ANY terminal invalid order (broker rejection or
                // exhausted submission retries) is surfaced as an error, matching
                // C# LEAN's `_algorithm.Error(...)` on the submit-failure path.
                tracing::error!(
                    "order {order_id} ({}) invalid: {message}",
                    order.symbol.value
                );
                order.status = OrderStatus::Invalid;
                transactions.add_or_update_order(order.clone());
                self.submit_attempts.remove(&order_id);
                self.retry_not_before.remove(&order_id);
                self.moo_deferred_logged.remove(&order_id);
                let order_event =
                    OrderEvent::invalid(order.id, order.symbol.clone(), DateTime::now(), message);
                self.emit(
                    order_event,
                    algorithm_manager,
                    services,
                    transactions,
                    all_order_events,
                );
            }
            BrokerageEvent::SubmitFailed { order_id, message } => {
                let Some(order) = transactions.get_order(order_id) else {
                    return;
                };
                // The submit attempt just finished, so it is no longer in flight.
                self.submitted_ids.remove(&order_id);
                let attempts = self.submit_attempts.get(&order_id).copied().unwrap_or(0);
                let max_attempts = self.submit_backoff.len() as u32 + 1;
                // `attempts` is the count already dispatched (incremented in
                // `dispatch_pending`). The delay before attempt N+1 is the Nth
                // backoff entry.
                if let Some(delay) = self.submit_backoff.get(attempts.saturating_sub(1) as usize) {
                    tracing::warn!(
                        "order {order_id} ({}) submission failed (attempt {attempts}/{max_attempts}), \
                         retrying in {}s: {message}",
                        order.symbol.value,
                        delay.as_secs(),
                    );
                    // Schedule the resubmission for a future iteration of
                    // `dispatch_pending`; the worker is free to service other
                    // orders in the meantime.
                    self.retry_not_before
                        .insert(order_id, std::time::Instant::now() + *delay);
                } else {
                    // Attempts exhausted: go terminally Invalid, reusing the same
                    // Invalid handling so the order event/log path is identical.
                    let final_message = format!(
                        "Brokerage order submission failed after {attempts} attempts: {message}"
                    );
                    self.apply_event(
                        BrokerageEvent::Invalid {
                            order_id,
                            message: final_message,
                        },
                        algorithm_manager,
                        services,
                        transactions,
                        portfolio,
                        all_order_events,
                        trade_builder,
                        completed_trades,
                    );
                }
            }
            BrokerageEvent::Status {
                order_id,
                symbol,
                status,
                fill_price,
                cumulative_filled,
                commission,
            } => {
                let Some(order) = transactions.get_order(order_id) else {
                    return;
                };
                // Defense in depth (issue #33): the brokerage-reported symbol for
                // this pushed order MUST match the engine order the id resolved
                // to. If a brokerage id ever mis-resolves to the wrong engine
                // order (id collision, map overwrite, adapter bug), applying the
                // fill would corrupt a different symbol's holdings. Refuse: log an
                // ERROR with both symbols/ids and do not apply the event or touch
                // any tracked state for this pairing. Cross-symbol application is
                // impossible regardless of id bookkeeping.
                if symbol != order.symbol {
                    tracing::error!(
                        "brokerage order/engine order symbol mismatch; refusing to apply fill: \
                         engine_order_id={order_id} engine_symbol={} brokerage_symbol={} \
                         status={status:?} cumulative_filled={cumulative_filled}",
                        order.symbol.value,
                        symbol.value,
                    );
                    return;
                }
                // Ignore stale/no-op transitions: the order already reached this
                // (or a later) terminal state.
                if order.status == status && !status.is_fill() {
                    return;
                }

                let previously_applied = self
                    .applied_filled
                    .get(&order_id)
                    .copied()
                    .unwrap_or_default();
                let fill_delta = cumulative_filled - previously_applied;
                let previously_applied_fee = self
                    .applied_fees
                    .get(&order_id)
                    .copied()
                    .unwrap_or_default();
                let fee_delta =
                    commission.map(|cumulative_fee| cumulative_fee - previously_applied_fee);

                let mut order_event =
                    OrderEvent::new(order.id, order.symbol.clone(), DateTime::now(), status);
                order_event.apply_order_fields(&order);
                order_event.fill_price = fill_price;
                order_event.fill_quantity = fill_delta;
                if let Some(fee_delta) = fee_delta {
                    order_event.order_fee = fee_delta;
                }

                if status.is_fill() && !fill_delta.is_zero() {
                    let fee = fee_delta.unwrap_or_else(|| {
                        let fill_order = fee_model_order_for_fill(&order, fill_delta);
                        algorithm_manager
                            .algorithm
                            .order_fee(&fill_order, order_event.fill_price)
                    });
                    order_event.order_fee = fee;
                    self.settle_fill(
                        &order,
                        &mut order_event,
                        algorithm_manager,
                        portfolio,
                        trade_builder,
                        completed_trades,
                    );
                    self.cash_sync.record_fill(DateTime::now());
                    self.applied_filled.insert(order_id, cumulative_filled);
                    if let Some(cumulative_fee) = commission {
                        self.applied_fees.insert(order_id, cumulative_fee);
                    }
                } else if !status.is_fill() {
                    order_event.message = status_message(status);
                }

                self.emit(
                    order_event,
                    algorithm_manager,
                    services,
                    transactions,
                    all_order_events,
                );
            }
            BrokerageEvent::CashSnapshot { cash_balances } => {
                let now = DateTime::now();
                let cash = rlean_live::account_sync::settlement_cash(&cash_balances);
                if let Some(portfolio) = portfolio {
                    let previous_cash = *portfolio.cash.read();
                    let previous_value = portfolio.total_portfolio_value();
                    let delta = cash - previous_cash;
                    *portfolio.cash.write() = cash;
                    let material_threshold = previous_value.abs() * Decimal::new(2, 2);
                    if delta.abs() > material_threshold {
                        tracing::warn!(
                            previous_cash = %previous_cash,
                            brokerage_cash = %cash,
                            delta = %delta,
                            portfolio_value = %previous_value,
                            "authoritative brokerage cash synchronization applied a material correction"
                        );
                    } else {
                        tracing::info!(
                            previous_cash = %previous_cash,
                            brokerage_cash = %cash,
                            delta = %delta,
                            "authoritative brokerage cash synchronization applied"
                        );
                    }
                }
                self.cash_sync.request_succeeded(now);
            }
            BrokerageEvent::CashSyncFailed { message } => {
                let now = DateTime::now();
                self.cash_sync.request_failed(now);
                tracing::warn!(
                    attempt = self.cash_sync.failures,
                    "brokerage cash synchronization failed; trading remains active and the sync will retry: {message}"
                );
            }
        }
    }

    /// Apply a fill's cash/holdings effect to the portfolio. The caller has
    /// already resolved `event.order_fee` from either the brokerage's exact
    /// cumulative fee delta or the security's fill-sized fee model.
    fn settle_fill<B: AlgorithmBridge>(
        &self,
        order: &Order,
        event: &mut OrderEvent,
        algorithm_manager: &AlgorithmManager<B>,
        portfolio: Option<&Arc<SecurityPortfolioManager>>,
        trade_builder: &mut TradeBuilder,
        completed_trades: &mut Vec<Trade>,
    ) {
        let bridge = &algorithm_manager.algorithm;
        let fee = event.order_fee;

        let Some(portfolio) = portfolio else {
            return;
        };
        let contract_multiplier = bridge.contract_multiplier_for_symbol(&order.symbol);
        portfolio.apply_fill_with_multiplier(
            &order.symbol,
            event.fill_price,
            event.fill_quantity,
            fee,
            contract_multiplier,
        );
        record_trade_fill(trade_builder, completed_trades, event, fee);
    }

    /// Persist the event into the transaction manager, invoke the algorithm
    /// bridge, and record it in the running order-event log.
    fn emit<B: AlgorithmBridge>(
        &self,
        event: OrderEvent,
        algorithm_manager: &mut AlgorithmManager<B>,
        services: &mut dyn AlgorithmServices,
        transactions: &Arc<TransactionManager>,
        all_order_events: &mut Vec<OrderEvent>,
    ) {
        transactions.process_order_event(event.clone());
        algorithm_manager.algorithm.on_order_event(&event, services);
        all_order_events.push(event);
    }

    /// Signal the worker to stop and join it.
    pub fn shutdown(&mut self) {
        let _ = self.request_tx.send(BrokerageRequest::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for LiveBrokerageRouter {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn status_message(status: OrderStatus) -> String {
    match status {
        OrderStatus::Canceled => "Brokerage order canceled".to_string(),
        OrderStatus::Invalid => "Brokerage rejected order".to_string(),
        OrderStatus::Submitted => "Brokerage order submitted".to_string(),
        other => format!("Brokerage order status {other:?}"),
    }
}

/// Poll cadence for brokerage account-order reconciliation.
#[cfg(test)]
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// Re-establish a sidecar brokerage connection after its event stream dropped.
///
/// First re-establishes the shared Flight session (coalesced with the data
/// feed's own reconnect), then re-opens the brokerage connection to obtain a
/// fresh event stream. Uses the same bounded 1s→60s backoff and retries a
/// restarting sidecar forever; a non-transient failure (bad credentials,
/// misconfiguration) is surfaced instead of looping.
async fn reopen_sidecar_brokerage(
    client: &rlean_data_sidecar::DataSidecarClient,
    failed_epoch: u64,
    provider: &str,
    opaque_config_json: &[u8],
) -> anyhow::Result<(u64, rlean_data_sidecar::BrokerageEventStream, u64)> {
    let policy = rlean_live::ReconnectPolicy::sidecar_session();
    let mut attempt: u32 = 0;
    loop {
        let outcome = async {
            client.reconnect_failed_epoch(failed_epoch).await?;
            let connection = client
                .open_brokerage(provider.to_string(), opaque_config_json.to_vec())
                .await?;
            Ok::<_, anyhow::Error>((connection.0, connection.1, client.session_epoch()))
        }
        .await;
        match outcome {
            Ok(connection) => return Ok(connection),
            Err(error) => {
                if !rlean_live::is_transient_sidecar_error(&error) {
                    return Err(error);
                }
                attempt = attempt.saturating_add(1);
                let delay = policy.delay_for_attempt(attempt - 1);
                tracing::warn!(
                    attempt,
                    delay_secs = delay.as_secs(),
                    brokerage = %provider,
                    "sidecar brokerage down; retrying reconnect: {error}"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

fn run_sidecar_worker(
    mut connection: crate::runner_config::SidecarBrokerageConnection,
    request_rx: Receiver<BrokerageRequest>,
    event_tx: Sender<BrokerageEvent>,
) {
    use futures::StreamExt;

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("live sidecar brokerage worker failed to start runtime: {error}");
            return;
        }
    };
    runtime.block_on(async move {
        let client = connection.client.clone();
        // Updated when the brokerage connection is re-opened after a sidecar
        // restart. The logical session id stays stable for idempotent commands;
        // each underlying Flight transport generation has its own protocol id.
        let mut connection_id = connection.connection_id;
        // Epoch that owns `connection.events`. A concurrent data-stream
        // reconnect may already have advanced the shared session by the time
        // this stream reports its failure; retaining the owning epoch prevents
        // the brokerage worker from reconnecting that fresh session again.
        let mut connection_epoch = connection.session_epoch;
        let session_id = client.session_id().to_string();
        let mut stop = false;
        while !stop {
            loop {
                match request_rx.try_recv() {
                    Ok(BrokerageRequest::Stop) => {
                        stop = true;
                        break;
                    }
                    Ok(BrokerageRequest::Submit(order)) => {
                        let command_id =
                            format!("{session_id}:{connection_id}:submit:{}", order.id);
                        let order_id = order.id;
                        match client
                            .submit_order(connection_id, command_id, (&order).into())
                            .await
                        {
                            Ok(result) if result.accepted => {
                                let _ = event_tx.send(BrokerageEvent::Submitted {
                                    order_id,
                                    brokerage_ids: result.brokerage_order_ids,
                                });
                            }
                            Ok(result) if result.retryable => {
                                let _ = event_tx.send(BrokerageEvent::SubmitFailed {
                                    order_id,
                                    message: result.message,
                                });
                            }
                            Ok(result) => {
                                let _ = event_tx.send(BrokerageEvent::Invalid {
                                    order_id,
                                    message: result.message,
                                });
                            }
                            Err(error) => {
                                let _ = event_tx.send(BrokerageEvent::SubmitFailed {
                                    order_id,
                                    message: error.to_string(),
                                });
                            }
                        }
                    }
                    Ok(BrokerageRequest::Update(order)) => {
                        let command_id =
                            format!("{session_id}:{connection_id}:update:{}", order.id);
                        match client
                            .update_order(connection_id, command_id, (&order).into())
                            .await
                        {
                            Ok(result) if result.accepted => {}
                            Ok(result) => tracing::warn!(
                                order_id = order.id,
                                retryable = result.retryable,
                                "sidecar brokerage rejected order update: {}",
                                result.message
                            ),
                            Err(error) => tracing::warn!(
                                order_id = order.id,
                                "sidecar brokerage update failed: {error}"
                            ),
                        }
                    }
                    Ok(BrokerageRequest::Cancel(order)) => {
                        let command_id =
                            format!("{session_id}:{connection_id}:cancel:{}", order.id);
                        match client
                            .cancel_order(
                                connection_id,
                                command_id,
                                order.id,
                                order.brokerage_id.clone(),
                            )
                            .await
                        {
                            Ok(result) if result.accepted => {}
                            Ok(result) => tracing::warn!(
                                order_id = order.id,
                                retryable = result.retryable,
                                "sidecar brokerage rejected order cancellation: {}",
                                result.message
                            ),
                            Err(error) => tracing::warn!(
                                order_id = order.id,
                                "sidecar brokerage cancellation failed: {error}"
                            ),
                        }
                    }
                    Ok(BrokerageRequest::CashSync) => {
                        let result = client
                            .brokerage_snapshot(connection_id)
                            .await
                            .and_then(|snapshot| {
                                snapshot
                                    .cash
                                    .into_iter()
                                    .map(|balance| {
                                        Ok((
                                            balance.currency,
                                            balance.amount.parse().with_context(|| {
                                                format!(
                                                    "invalid brokerage cash amount '{}'",
                                                    balance.amount
                                                )
                                            })?,
                                        ))
                                    })
                                    .collect::<anyhow::Result<Vec<_>>>()
                            });
                        match result {
                            Ok(cash_balances) => {
                                let _ = event_tx.send(BrokerageEvent::CashSnapshot {
                                    cash_balances,
                                });
                            }
                            Err(error) => {
                                let _ = event_tx.send(BrokerageEvent::CashSyncFailed {
                                    message: error.to_string(),
                                });
                            }
                        }
                    }
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        stop = true;
                        break;
                    }
                }
            }
            if stop {
                break;
            }

            let mut reconnect_reason = None;
            tokio::select! {
                event = connection.events.next() => {
                    match event {
                        Some(Ok(SidecarBrokerageEvent::Order(update))) => {
                            let parsed = (|| -> anyhow::Result<BrokerageEvent> {
                                let symbol = symbol_from_wire(
                                    update.symbol.context("brokerage order update is missing symbol")?
                                )?;
                                let status = order_status_from_wire(update.status)?;
                                let cumulative_filled = update.cumulative_filled_quantity.parse()
                                    .context("invalid cumulative brokerage fill quantity")?;
                                let fill_price = update.average_fill_price.parse()
                                    .context("invalid brokerage average fill price")?;
                                let commission = update.cumulative_fee
                                    .as_deref()
                                    .map(str::parse)
                                    .transpose()
                                    .context("invalid cumulative brokerage fee")?;
                                Ok(BrokerageEvent::Status {
                                    order_id: update.engine_order_id,
                                    symbol,
                                    status,
                                    fill_price,
                                    cumulative_filled,
                                    commission,
                                })
                            })();
                            match parsed {
                                Ok(event) => { let _ = event_tx.send(event); }
                                Err(error) => tracing::error!("invalid sidecar brokerage update: {error}"),
                            }
                        }
                        Some(Ok(SidecarBrokerageEvent::Connection(state))) => {
                            if !state.connected {
                                reconnect_reason = Some(format!(
                                    "brokerage reported disconnected: {}",
                                    state.message
                                ));
                            }
                        }
                        Some(Err(error)) => {
                            reconnect_reason =
                                Some(format!("brokerage event stream failed: {error}"));
                        }
                        None => {
                            reconnect_reason =
                                Some("brokerage event stream closed".to_string());
                        }
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => {}
            }
            if let Some(reason) = reconnect_reason {
                tracing::warn!(
                    brokerage = %connection.name,
                    "{reason}; re-establishing"
                );
                match reopen_sidecar_brokerage(
                    &client,
                    connection_epoch,
                    &connection.name,
                    &connection.opaque_config_json,
                )
                .await
                {
                    Ok((new_id, events, new_epoch)) => {
                        connection_id = new_id;
                        connection_epoch = new_epoch;
                        connection.events = events;
                        tracing::warn!(
                            brokerage = %connection.name,
                            connection_id = new_id,
                            session_epoch = new_epoch,
                            "re-established sidecar brokerage event stream"
                        );
                    }
                    Err(error) => {
                        tracing::error!(
                            "sidecar brokerage reconnect is not retryable: {error}"
                        );
                        break;
                    }
                }
            }
        }
        if let Err(error) = client.close_brokerage(connection_id).await {
            tracing::warn!("failed to close sidecar brokerage connection: {error}");
        }
    });
}

/// Worker loop: owns the brokerage, services submission/cancel/update requests,
/// and polls order state. Runs on its own thread with a current-thread tokio
/// runtime so brokerage adapters that `block_on` internally work correctly.
#[cfg(test)]
fn run_worker(
    mut brokerage: Box<dyn Brokerage>,
    request_rx: Receiver<BrokerageRequest>,
    event_tx: Sender<BrokerageEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!("live brokerage worker failed to start runtime: {error}");
            return;
        }
    };
    let _guard = runtime.enter();

    let mut tracked: HashMap<i64, TrackedBrokerageOrder> = HashMap::new();
    // Map brokerage id -> engine order id, so polled account orders (which only
    // carry brokerage ids) can be reconciled back to engine orders.
    let mut brokerage_to_engine: HashMap<String, i64> = HashMap::new();
    let mut last_poll = std::time::Instant::now() - POLL_INTERVAL;

    loop {
        // Drain any pending requests without blocking longer than the poll gap.
        match request_rx.recv_timeout(std::time::Duration::from_millis(100)) {
            Ok(BrokerageRequest::Stop) => break,
            Ok(BrokerageRequest::Submit(order)) => {
                handle_submit(&mut *brokerage, order, &event_tx, &mut brokerage_to_engine);
            }
            Ok(BrokerageRequest::Cancel(order)) => {
                if let Err(error) = brokerage.cancel_order(&order) {
                    tracing::warn!("live brokerage cancel failed for {}: {error}", order.id);
                }
            }
            Ok(BrokerageRequest::Update(order)) => {
                if let Err(error) = brokerage.update_order(&order) {
                    tracing::warn!("live brokerage update failed for {}: {error}", order.id);
                }
            }
            Ok(BrokerageRequest::CashSync) => {
                let cash_balances = brokerage.get_cash_balance();
                let _ = event_tx.send(BrokerageEvent::CashSnapshot { cash_balances });
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = std::time::Instant::now();
            poll_account_orders(&*brokerage, &event_tx, &mut tracked, &brokerage_to_engine);
        }
    }
}

#[cfg(test)]
fn handle_submit(
    brokerage: &mut dyn Brokerage,
    order: Order,
    event_tx: &Sender<BrokerageEvent>,
    brokerage_to_engine: &mut HashMap<String, i64>,
) {
    let order_id = order.id;
    match brokerage.place_order_with_brokerage_ids(order) {
        Ok(Some(brokerage_ids)) => {
            for brokerage_id in &brokerage_ids {
                brokerage_to_engine.insert(brokerage_id.clone(), order_id);
            }
            let _ = event_tx.send(BrokerageEvent::Submitted {
                order_id,
                brokerage_ids,
            });
        }
        // Ok(None) means the brokerage declined without surfacing a reason (e.g.
        // the default trait impl returning `false`). Brokerages that know why a
        // submission failed return `Err`, which carries the real detail below.
        Ok(None) => {
            let _ = event_tx.send(BrokerageEvent::Invalid {
                order_id,
                message: "Brokerage rejected order submission (no reason reported)".to_string(),
            });
        }
        // An `Err` is a transport/transient submission failure (the adapter could
        // not complete the request), NOT a broker rejection. Report it as a
        // retryable `SubmitFailed`; the router retries a bounded number of times
        // before marking the order `Invalid`.
        Err(error) => {
            let _ = event_tx.send(BrokerageEvent::SubmitFailed {
                order_id,
                message: format!("Brokerage order submission failed: {error}"),
            });
        }
    }
}

/// Poll the brokerage for all account orders and emit an event for any that
/// changed status or accumulated additional fills.
#[cfg(test)]
fn poll_account_orders(
    brokerage: &dyn Brokerage,
    event_tx: &Sender<BrokerageEvent>,
    tracked: &mut HashMap<i64, TrackedBrokerageOrder>,
    brokerage_to_engine: &HashMap<String, i64>,
) {
    for account_order in brokerage.get_account_orders() {
        let Some(engine_order_id) = resolve_engine_order_id(&account_order, brokerage_to_engine)
        else {
            continue;
        };

        let entry = tracked.entry(engine_order_id).or_default();
        let status_changed = entry.status != Some(account_order.status);
        let fill_changed = account_order.filled_quantity != entry.filled;
        if !status_changed && !fill_changed {
            continue;
        }
        entry.status = Some(account_order.status);
        entry.filled = account_order.filled_quantity;

        let commission = None; // Brokerages that expose per-fill fees can set this later.
        let _ = event_tx.send(BrokerageEvent::Status {
            order_id: engine_order_id,
            // Thread the brokerage-reported symbol through so the loop can verify
            // it against the engine order before applying anything (issue #33).
            symbol: account_order.symbol.clone(),
            status: account_order.status,
            fill_price: account_order.average_fill_price,
            cumulative_filled: account_order.filled_quantity,
            commission,
        });
    }
}

/// Map a polled account order back to the engine order id. Uses the brokerage-id
/// map first, then falls back to the engine order id carried on the order when
/// the brokerage echoes it.
#[cfg(test)]
fn resolve_engine_order_id(
    order: &Order,
    brokerage_to_engine: &HashMap<String, i64>,
) -> Option<i64> {
    for brokerage_id in &order.brokerage_id {
        if let Some(engine_id) = brokerage_to_engine.get(brokerage_id) {
            return Some(*engine_id);
        }
    }
    // Some brokerages reuse the engine order id as the order id (e.g. paper
    // adapters); only honor that when it is a known engine order in the map's
    // value set, otherwise ignore unknown externally-created orders.
    if order.id > 0 && brokerage_to_engine.values().any(|id| *id == order.id) {
        return Some(order.id);
    }
    None
}

/// Result of syncing the brokerage account at startup.
#[derive(Debug, Clone, Default)]
pub struct StartupAccountSync {
    pub cash: Decimal,
    pub cash_balances: Vec<(String, Decimal)>,
    pub holdings: Vec<rlean_brokerages::BrokerageHolding>,
    pub open_orders: Vec<Order>,
}

/// Pull the initial account snapshot through the already-authenticated sidecar
/// brokerage connection and seed the engine-owned portfolio/order state.
pub async fn startup_sidecar_account_sync(
    connection: &crate::runner_config::SidecarBrokerageConnection,
    portfolio: Option<&Arc<SecurityPortfolioManager>>,
    transactions: Option<&Arc<TransactionManager>>,
) -> anyhow::Result<StartupAccountSync> {
    let snapshot = connection
        .client
        .brokerage_snapshot(connection.connection_id)
        .await?;
    let cash_balances = snapshot
        .cash
        .into_iter()
        .map(|balance| {
            Ok((
                balance.currency,
                balance.amount.parse().with_context(|| {
                    format!("invalid brokerage cash amount '{}'", balance.amount)
                })?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let holdings = snapshot
        .holdings
        .into_iter()
        .map(|holding| {
            Ok(rlean_brokerages::BrokerageHolding {
                symbol: symbol_from_wire(
                    holding
                        .symbol
                        .context("brokerage holding is missing its symbol")?,
                )?,
                quantity: holding
                    .quantity
                    .parse()
                    .context("invalid brokerage holding quantity")?,
                average_price: holding
                    .average_price
                    .parse()
                    .context("invalid brokerage holding average price")?,
                market_price: holding
                    .market_price
                    .parse()
                    .context("invalid brokerage holding market price")?,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let open_orders = snapshot
        .open_orders
        .into_iter()
        .map(Order::try_from)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let cash = rlean_live::account_sync::settlement_cash(&cash_balances);

    if let Some(portfolio) = portfolio {
        if !cash_balances.is_empty() {
            *portfolio.cash.write() = cash;
        }
        for holding in &holdings {
            if holding.quantity.is_zero() {
                continue;
            }
            let multiplier = rlean_algorithm::portfolio::SecurityHolding::infer_contract_multiplier(
                &holding.symbol,
            );
            portfolio.set_holdings(
                &holding.symbol,
                holding.average_price,
                holding.quantity,
                multiplier,
            );
            if holding.market_price > Decimal::ZERO {
                portfolio.update_prices(&holding.symbol, holding.market_price);
            }
        }
    }
    if let Some(transactions) = transactions {
        for order in &open_orders {
            transactions.add_or_update_order(order.clone());
        }
    }
    tracing::info!(
        "Brokerage account sync: brokerage={} cash={cash} currencies={} holdings={} open_orders={}",
        connection.name,
        cash_balances.len(),
        holdings.len(),
        open_orders.len(),
    );
    Ok(StartupAccountSync {
        cash,
        cash_balances,
        holdings,
        open_orders,
    })
}

/// Connect the brokerage and pull cash, holdings and open orders, seeding the
/// portfolio and transaction manager. Mirrors C# `BrokerageSetupHandler`.
///
/// Runs the blocking brokerage calls on a dedicated thread so we do not stall
/// the async runtime, matching the worker's isolation model.
#[cfg(test)]
pub fn startup_account_sync(
    brokerage: &mut Box<dyn Brokerage>,
    portfolio: Option<&Arc<SecurityPortfolioManager>>,
    transactions: Option<&Arc<TransactionManager>>,
) -> anyhow::Result<StartupAccountSync> {
    brokerage
        .connect()
        .map_err(|error| anyhow::anyhow!("brokerage connect failed: {error}"))?;

    let cash_balances = brokerage.get_cash_balance();
    let cash = rlean_live::account_sync::settlement_cash(&cash_balances);
    let holdings = brokerage.get_account_detailed_holdings();
    let open_orders = brokerage.get_open_orders();

    if let Some(portfolio) = portfolio {
        if !cash_balances.is_empty() {
            *portfolio.cash.write() = cash;
        }
        for holding in &holdings {
            if holding.quantity.is_zero() {
                continue;
            }
            let multiplier = rlean_algorithm::portfolio::SecurityHolding::infer_contract_multiplier(
                &holding.symbol,
            );
            portfolio.set_holdings(
                &holding.symbol,
                holding.average_price,
                holding.quantity,
                multiplier,
            );
            if holding.market_price > Decimal::ZERO {
                portfolio.update_prices(&holding.symbol, holding.market_price);
            }
        }
    }

    if let Some(transactions) = transactions {
        for order in &open_orders {
            transactions.add_or_update_order(order.clone());
        }
    }

    tracing::info!(
        "Brokerage account sync: brokerage={} cash={cash} currencies={} holdings={} open_orders={}",
        brokerage.name(),
        cash_balances.len(),
        holdings.len(),
        open_orders.len(),
    );

    Ok(StartupAccountSync {
        cash,
        cash_balances,
        holdings,
        open_orders,
    })
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use parking_lot::Mutex as PlMutex;
    use rlean_brokerages::BrokerageHolding;
    use rlean_core::{Result as LeanResult, Symbol};

    /// Deterministic mock brokerage for router/startup-sync tests.
    ///
    /// `submit_should_fail` makes `place_order_with_brokerage_ids` return `None`
    /// (terminal rejection). `submit_err_count` makes it return `Err` (a
    /// transient submission failure) that many times, decrementing on each call,
    /// then succeed — used to exercise the bounded submission retry.
    /// `account_orders` is what the poll returns; tests mutate it to simulate a
    /// fill after submission.
    #[derive(Default)]
    pub struct MockBrokerage {
        pub connected: bool,
        pub submit_should_fail: bool,
        pub submit_err_count: Arc<PlMutex<u32>>,
        pub next_brokerage_id: PlMutex<i64>,
        pub submitted: Arc<PlMutex<Vec<Order>>>,
        pub account_orders: Arc<PlMutex<Vec<Order>>>,
        pub cash: Vec<(String, Price)>,
        pub detailed_holdings: Vec<BrokerageHolding>,
        pub open_orders: Vec<Order>,
    }

    impl MockBrokerage {
        pub fn new() -> Self {
            Self {
                next_brokerage_id: PlMutex::new(1000),
                submitted: Arc::new(PlMutex::new(Vec::new())),
                account_orders: Arc::new(PlMutex::new(Vec::new())),
                cash: vec![("USD".to_string(), Decimal::ZERO)],
                ..Default::default()
            }
        }
    }

    impl Brokerage for MockBrokerage {
        fn name(&self) -> &str {
            "MockBrokerage"
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn connect(&mut self) -> LeanResult<()> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) {
            self.connected = false;
        }
        fn place_order(&mut self, order: Order) -> LeanResult<bool> {
            Ok(self.place_order_with_brokerage_ids(order)?.is_some())
        }
        fn place_order_with_brokerage_ids(
            &mut self,
            order: Order,
        ) -> LeanResult<Option<Vec<String>>> {
            // A pending transient-failure budget returns `Err` (a submission
            // failure that the router retries), decrementing each call.
            {
                let mut remaining = self.submit_err_count.lock();
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err(rlean_core::LeanError::BrokerageError(
                        "transient submit failure".to_string(),
                    ));
                }
            }
            if self.submit_should_fail {
                return Ok(None);
            }
            let brokerage_id = {
                let mut id = self.next_brokerage_id.lock();
                *id += 1;
                id.to_string()
            };
            self.submitted.lock().push(order.clone());
            // Reflect the submitted order into the account-orders view as
            // Submitted so a subsequent poll can transition it to Filled.
            let mut reflected = order;
            reflected.status = OrderStatus::Submitted;
            reflected.brokerage_id = vec![brokerage_id.clone()];
            self.account_orders.lock().push(reflected);
            Ok(Some(vec![brokerage_id]))
        }
        fn update_order(&mut self, _order: &Order) -> LeanResult<bool> {
            Ok(true)
        }
        fn cancel_order(&mut self, _order: &Order) -> LeanResult<bool> {
            Ok(true)
        }
        fn get_open_orders(&self) -> Vec<Order> {
            self.open_orders.clone()
        }
        fn get_account_orders(&self) -> Vec<Order> {
            self.account_orders.lock().clone()
        }
        fn get_cash_balance(&self) -> Vec<(String, Price)> {
            self.cash.clone()
        }
        fn get_account_holdings(&self) -> HashMap<Symbol, Quantity> {
            self.detailed_holdings
                .iter()
                .map(|holding| (holding.symbol.clone(), holding.quantity))
                .collect()
        }
        fn get_account_detailed_holdings(&self) -> Vec<BrokerageHolding> {
            self.detailed_holdings.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::MockBrokerage;
    use super::*;
    use rlean_algorithm::qc_algorithm::{AccountType, BrokerageName, QcAlgorithm};
    use rlean_brokerages::BrokerageHolding;
    use rlean_core::{DateTime, Market, Resolution, Symbol};
    use rust_decimal_macros::dec;

    fn ny_time(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime {
        use chrono::TimeZone;
        rlean_core::time::tz::NEW_YORK
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .unwrap()
            .with_timezone(&chrono::Utc)
            .into()
    }

    fn spy() -> Symbol {
        Symbol::create_equity("SPY", &Market::usa())
    }

    #[test]
    fn live_cash_sync_matches_lean_daily_schedule_and_fill_quiet_period() {
        let startup = ny_time(2026, 7, 29, 6, 0);
        let mut state = LiveCashSyncState::new(startup);

        // Startup synchronization covers the current day.
        assert!(!state.should_request(ny_time(2026, 7, 29, 8, 0)));
        assert!(!state.should_request(ny_time(2026, 7, 30, 7, 44)));
        assert!(state.should_request(ny_time(2026, 7, 30, 7, 45)));

        state.request_started();
        let synced = ny_time(2026, 7, 30, 7, 45);
        state.request_succeeded(synced);
        state.record_fill(rlean_core::NanosecondTimestamp(synced.0 + 5_000_000_000));

        // Ten seconds after the snapshot, the recent fill invalidates it.
        assert!(!state.should_request(rlean_core::NanosecondTimestamp(
            synced.0 + CASH_SYNC_VERIFY_DELAY_NS
        )));
        // Once the fill is more than ten seconds old, the same day is eligible
        // again and the authoritative snapshot is retried.
        assert!(state.should_request(rlean_core::NanosecondTimestamp(synced.0 + 21_000_000_000)));
    }

    #[test]
    fn live_cash_sync_failure_retries_without_terminating_trading() {
        let now = ny_time(2026, 7, 30, 7, 45);
        let mut state = LiveCashSyncState::new(ny_time(2026, 7, 29, 8, 0));
        assert!(state.should_request(now));
        state.request_started();
        state.request_failed(now);

        assert_eq!(state.failures, 1);
        assert!(!state.request_in_flight);
        assert!(!state.should_request(rlean_core::NanosecondTimestamp(now.0 + 999_000_000)));
        assert!(state.should_request(rlean_core::NanosecondTimestamp(now.0 + 1_000_000_000)));
    }

    #[test]
    fn fee_model_fallback_prices_only_each_partial_fill() {
        let order = Order::market(1, spy(), dec!(10), DateTime::now(), "");
        let first = fee_model_order_for_fill(&order, dec!(3));
        let second = fee_model_order_for_fill(&order, dec!(7));

        assert_eq!(first.quantity, dec!(3));
        assert_eq!(second.quantity, dec!(7));
        assert_eq!(first.filled_quantity, Decimal::ZERO);
        assert_eq!(second.filled_quantity, Decimal::ZERO);
        assert_eq!(first.quantity + second.quantity, order.quantity);
    }

    #[test]
    fn live_cash_sync_request_uses_worker_owned_brokerage_connection() {
        let mock = MockBrokerage {
            cash: vec![("USD".to_string(), dec!(12345.67))],
            ..MockBrokerage::new()
        };
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));
        router.cash_sync = LiveCashSyncState::new(ny_time(2026, 7, 29, 8, 0));
        router.request_cash_sync_if_due(ny_time(2026, 7, 30, 7, 45));

        let event = router
            .event_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("cash snapshot event");
        assert!(matches!(
            event,
            BrokerageEvent::CashSnapshot { cash_balances }
                if cash_balances == vec![("USD".to_string(), dec!(12345.67))]
        ));
        router.shutdown();
    }

    #[test]
    fn startup_sync_seeds_cash_holdings_and_open_orders() {
        let mut brokerage: Box<dyn Brokerage> = Box::new(MockBrokerage {
            cash: vec![("USD".to_string(), dec!(50000))],
            detailed_holdings: vec![BrokerageHolding {
                symbol: spy(),
                quantity: dec!(10),
                average_price: dec!(400),
                market_price: dec!(410),
            }],
            open_orders: vec![Order::market(7, spy(), dec!(5), DateTime::now(), "resume")],
            ..MockBrokerage::new()
        });
        let portfolio = Arc::new(SecurityPortfolioManager::new_live(dec!(0)));
        let transactions = Arc::new(TransactionManager::new());

        let sync =
            startup_account_sync(&mut brokerage, Some(&portfolio), Some(&transactions)).unwrap();

        assert!(brokerage.is_connected());
        assert_eq!(sync.cash, dec!(50000));
        assert_eq!(*portfolio.cash.read(), dec!(50000));
        let holding = portfolio.get_holding(&spy());
        assert_eq!(holding.quantity, dec!(10));
        assert_eq!(holding.average_price, dec!(400));
        // Open order registered in the transaction manager.
        assert!(transactions.get_order(7).is_some());
    }

    #[test]
    fn resolve_engine_order_id_maps_via_brokerage_id() {
        let mut map = HashMap::new();
        map.insert("BID-1".to_string(), 42_i64);
        let mut order = Order::market(999, spy(), dec!(1), DateTime::now(), "");
        order.brokerage_id = vec!["BID-1".to_string()];
        assert_eq!(resolve_engine_order_id(&order, &map), Some(42));

        // Unknown externally-created order is ignored.
        let unknown = Order::market(5, spy(), dec!(1), DateTime::now(), "");
        assert_eq!(resolve_engine_order_id(&unknown, &map), None);
    }

    #[test]
    fn cash_buying_power_rejection_never_crosses_brokerage_boundary() {
        let mut algorithm = QcAlgorithm::new("test", dec!(8226));
        algorithm.set_brokerage_model(BrokerageName::RobinhoodBrokerage, AccountType::Cash);

        let existing = algorithm.add_equity("SPY", Resolution::Minute);
        algorithm.securities.update_price(&existing, dec!(100));
        algorithm
            .portfolio
            .set_holdings(&existing, dec!(100), dec!(800), dec!(1));

        let replacement = algorithm.add_equity("FHN", Resolution::Minute);
        algorithm.securities.update_price(&replacement, dec!(25.39));
        let order = Order::market(1, replacement, dec!(419), DateTime::now(), "replacement");
        algorithm.transactions.add_order(order);

        let mock = MockBrokerage::new();
        let submitted = mock.submitted.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));
        let events = router.dispatch_pending(&algorithm.transactions, &algorithm);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].status, OrderStatus::Invalid);
        assert!(events[0].message.contains("Insufficient buying power"));
        assert!(submitted.lock().is_empty());
        assert_eq!(
            algorithm.transactions.get_order(1).unwrap().status,
            OrderStatus::Invalid
        );
        router.shutdown();
    }

    #[test]
    fn zero_price_position_reduction_waits_for_price_instead_of_becoming_invalid() {
        let mut algorithm = QcAlgorithm::new("test", dec!(10000));
        algorithm.set_brokerage_model(BrokerageName::TradierBrokerage, AccountType::Margin);

        let existing = algorithm.add_equity("SPY", Resolution::Minute);
        algorithm
            .portfolio
            .set_holdings(&existing, dec!(100), dec!(10), dec!(1));
        let sell = Order::market(
            1,
            existing.clone(),
            dec!(-10),
            DateTime::now(),
            "Liquidate unmanaged holding",
        );
        algorithm.transactions.add_order(sell);

        let mock = MockBrokerage::new();
        let submitted = mock.submitted.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));

        let events = router.dispatch_pending(&algorithm.transactions, &algorithm);
        assert!(events.is_empty());
        assert!(submitted.lock().is_empty());
        assert_eq!(
            algorithm.transactions.get_order(1).unwrap().status,
            OrderStatus::New
        );

        algorithm.securities.update_price(&existing, dec!(101));
        algorithm.portfolio.update_prices(&existing, dec!(101));
        let events = router.dispatch_pending(&algorithm.transactions, &algorithm);
        assert!(events.is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while submitted.lock().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(submitted.lock().len(), 1);
        assert_eq!(submitted.lock()[0].symbol, existing);
        router.shutdown();
    }

    #[test]
    fn cash_rebalance_dispatches_reduction_before_replacement_buy() {
        let mut algorithm = QcAlgorithm::new("test", dec!(8226));
        algorithm.set_brokerage_model(BrokerageName::RobinhoodBrokerage, AccountType::Cash);

        let existing = algorithm.add_equity("SPY", Resolution::Minute);
        algorithm.securities.update_price(&existing, dec!(800));
        algorithm
            .portfolio
            .set_holdings(&existing, dec!(100), dec!(800), dec!(1));
        let replacement = algorithm.add_equity("FHN", Resolution::Minute);
        algorithm.securities.update_price(&replacement, dec!(25.39));

        let sell = Order::market(1, existing.clone(), dec!(-100), DateTime::now(), "replace");
        let buy = Order::market(
            2,
            replacement.clone(),
            dec!(419),
            DateTime::now(),
            "replace",
        );
        algorithm.transactions.add_order(sell.clone());
        algorithm.transactions.add_order(buy);

        let mock = MockBrokerage::new();
        let submitted = mock.submitted.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));
        let events = router.dispatch_pending(&algorithm.transactions, &algorithm);
        assert!(events.is_empty());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while submitted.lock().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(submitted.lock().len(), 1);
        assert_eq!(submitted.lock()[0].symbol, existing);
        assert_eq!(
            algorithm.transactions.get_order(2).unwrap().status,
            OrderStatus::New
        );

        // Once the sale fill is reconciled, its proceeds are real cash and the
        // still-New replacement order is dispatched immediately by the next
        // brokerage-service pass (which the live loop runs every 250 ms).
        algorithm
            .portfolio
            .apply_fill(&sell, dec!(800), dec!(-100), Decimal::ZERO);
        algorithm
            .transactions
            .process_order_event(OrderEvent::filled(
                1,
                existing,
                DateTime::now(),
                dec!(800),
                dec!(-100),
            ));
        let events = router.dispatch_pending(&algorithm.transactions, &algorithm);
        assert!(events.is_empty());

        while submitted.lock().len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(submitted.lock().len(), 2);
        assert_eq!(submitted.lock()[1].symbol, replacement);
        router.shutdown();
    }

    /// End-to-end worker test: submit an order, then flip the account view to
    /// Filled, and assert the router surfaces a Submitted then a Filled event.
    /// Uses the low-level channels via a helper that drains raw events.
    #[test]
    fn worker_emits_submitted_then_filled() {
        let mock = MockBrokerage::new();
        let account_orders = mock.account_orders.clone();
        let mut router = LiveBrokerageRouter::spawn(Box::new(mock));

        // Submit a New order.
        let order = Order::market(1, spy(), dec!(10), DateTime::now(), "entry");
        router
            .request_tx
            .send(BrokerageRequest::Submit(order))
            .unwrap();

        // Wait for the Submitted event.
        let submitted = wait_for_event(&router.event_rx, |event| {
            matches!(event, BrokerageEvent::Submitted { order_id: 1, .. })
        });
        assert!(submitted, "expected Submitted event for order 1");

        // Flip the reflected account order to Filled and let the poll pick it up.
        {
            let mut orders = account_orders.lock();
            for order in orders.iter_mut() {
                order.status = OrderStatus::Filled;
                order.filled_quantity = dec!(10);
                order.average_fill_price = dec!(400);
            }
        }

        let filled = wait_for_event(&router.event_rx, |event| {
            matches!(
                event,
                BrokerageEvent::Status {
                    order_id: 1,
                    status: OrderStatus::Filled,
                    cumulative_filled,
                    ..
                } if *cumulative_filled == dec!(10)
            )
        });
        assert!(filled, "expected Filled status event for order 1");
        router.shutdown();
    }

    fn wait_for_event(
        rx: &Receiver<BrokerageEvent>,
        predicate: impl Fn(&BrokerageEvent) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(event) if predicate(&event) => return true,
                Ok(_) => continue,
                Err(_) => continue,
            }
        }
        false
    }
}
