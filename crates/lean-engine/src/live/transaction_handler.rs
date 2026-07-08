//! Engine-owned live transaction handler.
//!
//! Modeled on C# LEAN's `BrokerageTransactionHandler`. When a real brokerage is
//! attached and it does not use local paper fills, new orders created by the
//! algorithm/framework are submitted to the brokerage and fills/status changes
//! are detected by polling the brokerage — instead of being filled locally by
//! the `ImmediateFillModel`.
//!
//! Threading model mirrors `LiveCustomSubscriptionSet`: the `Box<dyn Brokerage>`
//! is owned by a dedicated worker thread (with its own current-thread tokio
//! runtime, since brokerage adapters `block_on` internally). The live loop
//! communicates with the worker over two crossbeam channels:
//!
//!   * request channel (loop -> worker): submit/cancel/update/stop.
//!   * event channel (worker -> loop): status + fill notifications.
//!
//! Portfolio, `TransactionManager` and algorithm callbacks are only ever touched
//! on the live-loop thread. The worker only talks to the brokerage.

use crossbeam_channel::{Receiver, Sender};
use lean_algorithm::lifecycle::{AlgorithmBridge, AlgorithmServices};
use lean_algorithm::portfolio::SecurityPortfolioManager;
use lean_brokerages::Brokerage;
use lean_core::{DateTime, Price, Quantity};
use lean_orders::{Order, OrderEvent, OrderStatus, TransactionManager};
use lean_statistics::{Trade, TradeBuilder};
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
    /// Shut the worker down.
    Stop,
}

/// A brokerage-originated notification for the live loop to apply.
#[derive(Debug, Clone)]
enum BrokerageEvent {
    /// The order was accepted by the brokerage; record its brokerage ids and
    /// transition New -> Submitted.
    Submitted {
        order_id: i64,
        brokerage_ids: Vec<String>,
    },
    /// Submission was rejected by the brokerage.
    Invalid { order_id: i64, message: String },
    /// A status/fill update polled from the brokerage. `cumulative_filled` is the
    /// signed total filled quantity the brokerage reports for the order.
    Status {
        order_id: i64,
        status: OrderStatus,
        fill_price: Price,
        cumulative_filled: Quantity,
        commission: Option<Price>,
    },
}

/// State the worker tracks per order it has seen from the brokerage, so it only
/// emits an event when something actually changed.
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
    /// polled snapshots only apply the incremental delta.
    applied_filled: HashMap<i64, Quantity>,
}

impl LiveBrokerageRouter {
    /// Spawn the worker thread and hand ownership of the brokerage to it.
    pub fn spawn(brokerage: Box<dyn Brokerage>) -> Self {
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
        }
    }

    /// Forward any `New` orders in the transaction manager to the brokerage, and
    /// forward pending cancel/update requests. Called once per live iteration on
    /// the loop thread; never blocks on the brokerage.
    pub fn dispatch_pending(&mut self, transactions: &Arc<TransactionManager>) {
        for order in transactions.get_open_orders() {
            match order.status {
                OrderStatus::New if self.submitted_ids.insert(order.id) => {
                    let _ = self.request_tx.send(BrokerageRequest::Submit(order));
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
    fn apply_event<B: AlgorithmBridge>(
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
                order.status = OrderStatus::Invalid;
                transactions.add_or_update_order(order.clone());
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
            BrokerageEvent::Status {
                order_id,
                status,
                fill_price,
                cumulative_filled,
                commission,
            } => {
                let Some(order) = transactions.get_order(order_id) else {
                    return;
                };
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

                let mut order_event =
                    OrderEvent::new(order.id, order.symbol.clone(), DateTime::now(), status);
                order_event.apply_order_fields(&order);
                order_event.fill_price = fill_price;
                order_event.fill_quantity = fill_delta;
                if let Some(commission) = commission {
                    order_event.order_fee = commission;
                }

                if status.is_fill() && !fill_delta.is_zero() {
                    self.settle_fill(
                        &order,
                        &mut order_event,
                        algorithm_manager,
                        portfolio,
                        trade_builder,
                        completed_trades,
                    );
                    self.applied_filled.insert(order_id, cumulative_filled);
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
        }
    }

    /// Apply a fill's cash/holdings effect to the portfolio, mirroring
    /// `settle_fill_event_bridge`: use the brokerage-reported commission when
    /// present, otherwise the security's fee model. Buying-power failures
    /// downgrade the event to Invalid (defensive; the brokerage already
    /// accepted it).
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
        let fee = if event.order_fee > Decimal::ZERO {
            event.order_fee
        } else {
            bridge.order_fee(order, event.fill_price)
        };
        event.order_fee = fee;

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
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// Worker loop: owns the brokerage, services submission/cancel/update requests,
/// and polls order state. Runs on its own thread with a current-thread tokio
/// runtime so brokerage adapters that `block_on` internally work correctly.
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
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }

        if last_poll.elapsed() >= POLL_INTERVAL {
            last_poll = std::time::Instant::now();
            poll_account_orders(&*brokerage, &event_tx, &mut tracked, &brokerage_to_engine);
        }
    }
}

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
        Err(error) => {
            let _ = event_tx.send(BrokerageEvent::Invalid {
                order_id,
                message: format!("Brokerage order submission failed: {error}"),
            });
        }
    }
}

/// Poll the brokerage for all account orders and emit an event for any that
/// changed status or accumulated additional fills.
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
fn resolve_engine_order_id(
    order: &Order,
    brokerage_to_engine: &HashMap<String, i64>,
) -> Option<i64> {
    for brokerage_id in &order.brokerage_id {
        if let Some(engine_id) = brokerage_to_engine.get(brokerage_id) {
            return Some(*engine_id);
        }
    }
    // Some plugins reuse the engine order id as the order id (e.g. paper
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
    pub holdings: Vec<lean_brokerages::BrokerageHolding>,
    pub open_orders: Vec<Order>,
}

/// Connect the brokerage and pull cash, holdings and open orders, seeding the
/// portfolio and transaction manager. Mirrors C# `BrokerageSetupHandler`.
///
/// Runs the blocking brokerage calls on a dedicated thread so we do not stall
/// the async runtime, matching the worker's isolation model.
pub fn startup_account_sync(
    brokerage: &mut Box<dyn Brokerage>,
    portfolio: Option<&Arc<SecurityPortfolioManager>>,
    transactions: Option<&Arc<TransactionManager>>,
) -> anyhow::Result<StartupAccountSync> {
    brokerage
        .connect()
        .map_err(|error| anyhow::anyhow!("brokerage connect failed: {error}"))?;

    let cash_balances = brokerage.get_cash_balance();
    let cash = lean_live::account_sync::settlement_cash(&cash_balances);
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
            let multiplier = lean_algorithm::portfolio::SecurityHolding::infer_contract_multiplier(
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
    use lean_brokerages::BrokerageHolding;
    use lean_core::{Result as LeanResult, Symbol};
    use parking_lot::Mutex as PlMutex;

    /// Deterministic mock brokerage for router/startup-sync tests.
    ///
    /// `submit_should_fail` makes `place_order_with_brokerage_ids` return `None`
    /// (rejection). `account_orders` is what the poll returns; tests mutate it to
    /// simulate a fill after submission.
    #[derive(Default)]
    pub struct MockBrokerage {
        pub connected: bool,
        pub submit_should_fail: bool,
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
    use lean_brokerages::BrokerageHolding;
    use lean_core::{DateTime, Market, Symbol};
    use rust_decimal_macros::dec;

    fn spy() -> Symbol {
        Symbol::create_equity("SPY", &Market::usa())
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
