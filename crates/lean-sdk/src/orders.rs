//! SDK-owned order request and transaction view APIs.
//!
//! All projection logic (decimal->f64, status mapping, synthetic OTM/expiry
//! event construction) lives here so language bindings are pure structural
//! glue that a generator can emit 1:1 from these getters.

use lean_algorithm::margin_call::MarginCallOrderRequest;
use lean_core::{DateTime, NanosecondTimestamp, Price, Symbol};
use lean_orders::order::{OrderDirection, OrderStatus};
use lean_orders::transaction_manager::TransactionManager;
use lean_orders::{Order, OrderEvent, OrderTicket, UpdateOrderFields};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct OrderSubmission {
    pub symbol: Symbol,
    pub quantity: Price,
    pub tag: String,
}

pub type MarginCallCallbackPayload = Vec<(String, f64)>;

pub fn margin_call_payload(requests: &[MarginCallOrderRequest]) -> MarginCallCallbackPayload {
    requests
        .iter()
        .map(|request| {
            (
                request.symbol.value.to_string(),
                request.quantity.to_f64().unwrap_or(0.0),
            )
        })
        .collect()
}

pub fn margin_call_requests_from_payload(
    items: MarginCallCallbackPayload,
) -> Vec<MarginCallOrderRequest> {
    items
        .into_iter()
        .filter_map(|(symbol, quantity)| {
            let quantity = Decimal::from_f64_retain(quantity)?;
            Some(MarginCallOrderRequest::new(
                Symbol::create_equity(&symbol, &lean_core::Market::usa()),
                quantity,
            ))
        })
        .collect()
}

pub fn margin_call_requests_from_orders(orders: &[Order]) -> Vec<MarginCallOrderRequest> {
    orders
        .iter()
        .map(|order| MarginCallOrderRequest::new(order.symbol.clone(), order.quantity))
        .collect()
}

pub fn margin_call_orders_from_requests(
    requests: Vec<MarginCallOrderRequest>,
    time: lean_core::DateTime,
) -> Vec<Order> {
    requests
        .into_iter()
        .map(|request| Order::market(0, request.symbol, request.quantity, time, request.tag))
        .collect()
}

pub fn otm_expiry_event_view(
    symbol: Symbol,
    utc_ns: i64,
    quantity: Decimal,
    underlying_price: Decimal,
    entry_premium: Decimal,
) -> OrderEventView {
    OrderEventView::otm_expiry(
        symbol,
        utc_ns,
        quantity,
        underlying_price.to_f64().unwrap_or(0.0),
        (entry_premium * quantity * rust_decimal_macros::dec!(100))
            .to_f64()
            .unwrap_or(0.0),
    )
}

pub fn assignment_expiry_event_view(
    symbol: Symbol,
    utc_ns: i64,
    quantity: Decimal,
    is_assignment: bool,
) -> OrderEventView {
    OrderEventView::expiry(symbol, utc_ns, quantity, is_assignment)
}

impl OrderSubmission {
    pub fn new(symbol: Symbol, quantity: Price, tag: impl Into<String>) -> Self {
        Self {
            symbol,
            quantity,
            tag: tag.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct OrderView {
    pub order: Order,
}

/// Maps a domain `OrderStatus` to the LEAN-compatible status integer.
/// `None`/`Invalid` collapse to `Invalid` to mirror LEAN's Python surface.
pub fn order_status_code(status: OrderStatus) -> i64 {
    match status {
        OrderStatus::New => 0,
        OrderStatus::Submitted => 1,
        OrderStatus::PartiallyFilled => 2,
        OrderStatus::Filled => 3,
        OrderStatus::Canceled => 5,
        OrderStatus::None => 6,
        OrderStatus::Invalid => 6,
        OrderStatus::CancelPending => 7,
        OrderStatus::UpdateSubmitted => 8,
    }
}

/// Maps a domain `OrderDirection` to the LEAN-compatible direction integer.
pub fn order_direction_code(direction: OrderDirection) -> i64 {
    match direction {
        OrderDirection::Buy => 0,
        OrderDirection::Sell => 1,
        OrderDirection::Hold => 2,
    }
}

#[derive(Debug, Clone, Default)]
pub struct OrderTicketUpdateRequest {
    pub limit_price: Option<f64>,
    pub stop_price: Option<f64>,
    pub tag: Option<String>,
}

impl OrderTicketUpdateRequest {
    pub fn new(limit_price: Option<f64>, stop_price: Option<f64>, tag: Option<String>) -> Self {
        Self {
            limit_price,
            stop_price,
            tag,
        }
    }

    fn into_fields(self) -> UpdateOrderFields {
        UpdateOrderFields {
            quantity: None,
            limit_price: self.limit_price.and_then(Decimal::from_f64_retain),
            stop_price: self.stop_price.and_then(Decimal::from_f64_retain),
            tag: self.tag,
        }
    }
}

pub trait OrderTicketTransactions {
    fn order_ticket_view(&self, order_id: i64) -> Option<OrderTicketView>;
    fn cancel_order_ticket(&self, order_id: i64, time: DateTime, tag: Option<String>) -> bool;
    fn update_order_ticket(
        &self,
        order_id: i64,
        time: DateTime,
        request: OrderTicketUpdateRequest,
    ) -> bool;
}

impl OrderTicketTransactions for TransactionManager {
    fn order_ticket_view(&self, order_id: i64) -> Option<OrderTicketView> {
        self.get_ticket(order_id).map(OrderTicketView::new)
    }

    fn cancel_order_ticket(&self, order_id: i64, time: DateTime, tag: Option<String>) -> bool {
        self.request_cancel_order(order_id, time, tag.unwrap_or_default())
    }

    fn update_order_ticket(
        &self,
        order_id: i64,
        time: DateTime,
        request: OrderTicketUpdateRequest,
    ) -> bool {
        self.request_update_order(order_id, time, request.into_fields())
    }
}

#[derive(Clone)]
pub struct OrderTicketContext {
    transactions: Arc<TransactionManager>,
    utc_time: Arc<dyn Fn() -> DateTime + Send + Sync>,
}

impl OrderTicketContext {
    pub fn new(
        transactions: Arc<TransactionManager>,
        utc_time: impl Fn() -> DateTime + Send + Sync + 'static,
    ) -> Self {
        Self {
            transactions,
            utc_time: Arc::new(utc_time),
        }
    }

    pub fn fixed(transactions: Arc<TransactionManager>, utc_time: DateTime) -> Self {
        Self::new(transactions, move || utc_time)
    }

    pub fn transactions(&self) -> &Arc<TransactionManager> {
        &self.transactions
    }

    pub fn utc_time(&self) -> DateTime {
        (self.utc_time)()
    }
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "OrderTicket"))]
pub struct OrderTicketHandle {
    order_id: i64,
    context: OrderTicketContext,
}

impl OrderTicketHandle {
    pub fn new(order_id: i64, context: OrderTicketContext) -> Self {
        Self { order_id, context }
    }

    pub fn from_ticket(ticket: OrderTicket, context: OrderTicketContext) -> Self {
        Self::new(ticket.order_id, context)
    }

    fn view(&self) -> Option<OrderTicketView> {
        self.context.transactions.order_ticket_view(self.order_id)
    }

    fn with_view<T>(&self, fallback: T, f: impl FnOnce(OrderTicketView) -> T) -> T {
        self.view().map(f).unwrap_or(fallback)
    }
    pub fn order_id(&self) -> i64 {
        self.order_id
    }
    pub fn id(&self) -> i64 {
        self.order_id
    }
    pub fn symbol(&self) -> Option<Symbol> {
        self.view().map(|view| view.symbol())
    }
    pub fn status_code(&self) -> i64 {
        self.with_view(order_status_code(OrderStatus::Invalid), |view| {
            view.status_code()
        })
    }
    pub fn quantity(&self) -> f64 {
        self.with_view(0.0, |view| view.quantity())
    }
    pub fn filled_quantity(&self) -> f64 {
        self.with_view(0.0, |view| view.filled_quantity())
    }
    pub fn average_fill_price(&self) -> f64 {
        self.with_view(0.0, |view| view.average_fill_price())
    }
    pub fn limit_price(&self) -> Option<f64> {
        self.view().and_then(|view| view.limit_price())
    }
    pub fn stop_price(&self) -> Option<f64> {
        self.view().and_then(|view| view.stop_price())
    }
    pub fn tag(&self) -> String {
        self.with_view(String::new(), |view| view.tag())
    }
    pub fn is_open(&self) -> bool {
        self.with_view(false, |view| view.is_open())
    }
    pub fn order_events(&self) -> Vec<OrderEventView> {
        self.with_view(Vec::new(), |view| view.order_events())
    }

    pub fn cancel(&self, tag: Option<String>) -> bool {
        self.context
            .transactions
            .cancel_order_ticket(self.order_id, self.context.utc_time(), tag)
    }

    pub fn update(
        &self,
        limit_price: Option<f64>,
        stop_price: Option<f64>,
        tag: Option<String>,
    ) -> bool {
        self.context.transactions.update_order_ticket(
            self.order_id,
            self.context.utc_time(),
            OrderTicketUpdateRequest::new(limit_price, stop_price, tag),
        )
    }

    pub fn update_limit_price(&self, limit_price: f64, tag: Option<String>) -> bool {
        self.update(Some(limit_price), None, tag)
    }

    pub fn update_stop_price(&self, stop_price: f64, tag: Option<String>) -> bool {
        self.update(None, Some(stop_price), tag)
    }

    pub fn update_tag(&self, tag: String) -> bool {
        self.update(None, None, Some(tag))
    }
}

/// Maps a LEAN-compatible status integer to the python `OrderStatus` enum.
#[cfg(feature = "python")]
fn order_status_from_code(code: i64) -> crate::types::OrderStatus {
    use crate::types::OrderStatus;
    match code {
        0 => OrderStatus::New,
        1 => OrderStatus::Submitted,
        2 => OrderStatus::PartiallyFilled,
        3 => OrderStatus::Filled,
        5 => OrderStatus::Canceled,
        7 => OrderStatus::CancelPending,
        8 => OrderStatus::UpdateSubmitted,
        _ => OrderStatus::Invalid,
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl OrderTicketHandle {
    #[getter(order_id)]
    fn py_order_id(&self) -> i64 {
        self.order_id()
    }

    #[getter(id)]
    fn py_id(&self) -> i64 {
        self.id()
    }

    #[getter(symbol)]
    fn py_symbol(&self) -> Option<crate::securities::SymbolHandle> {
        self.symbol().map(crate::securities::SymbolHandle::new)
    }

    #[getter(status)]
    fn py_status(&self) -> crate::types::OrderStatus {
        order_status_from_code(self.status_code())
    }

    #[getter(quantity)]
    fn py_quantity(&self) -> f64 {
        self.quantity()
    }

    #[getter(quantity_filled)]
    fn py_quantity_filled(&self) -> f64 {
        self.filled_quantity()
    }

    #[getter(average_fill_price)]
    fn py_average_fill_price(&self) -> f64 {
        self.average_fill_price()
    }

    #[getter(limit_price)]
    fn py_limit_price(&self) -> Option<f64> {
        self.limit_price()
    }

    #[getter(stop_price)]
    fn py_stop_price(&self) -> Option<f64> {
        self.stop_price()
    }

    #[getter(tag)]
    fn py_tag(&self) -> String {
        self.tag()
    }

    #[pyo3(name = "get_order_events")]
    fn py_get_order_events(&self) -> Vec<OrderEventView> {
        self.order_events()
    }

    #[pyo3(name = "cancel", signature = (tag=None))]
    fn py_cancel(&self, tag: Option<String>) -> bool {
        self.cancel(tag)
    }

    #[pyo3(name = "update_limit_price", signature = (limit_price, tag=None))]
    fn py_update_limit_price(&self, limit_price: f64, tag: Option<String>) -> bool {
        self.update_limit_price(limit_price, tag)
    }

    #[pyo3(name = "update_stop_price", signature = (stop_price, tag=None))]
    fn py_update_stop_price(&self, stop_price: f64, tag: Option<String>) -> bool {
        self.update_stop_price(stop_price, tag)
    }

    #[pyo3(name = "update_tag")]
    fn py_update_tag(&self, tag: String) -> bool {
        self.update_tag(tag)
    }
}

/// SDK-owned projection of an `OrderEvent` for language bindings.
///
/// Every accessor returns a primitive (or `Symbol`) so a generator can emit a
/// Python getter per method with no hand-written conversion logic.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "OrderEvent"))]
pub struct OrderEventView {
    pub event: OrderEvent,
}

impl OrderEventView {
    pub fn new(event: OrderEvent) -> Self {
        Self { event }
    }

    /// Synthetic OTM-expiry event mirroring LEAN's `on_order_event` format:
    /// fill_price=0 and message `"OTM. Underlying: X. Profit: +Y"`.
    pub fn otm_expiry(
        symbol: Symbol,
        utc_ns: i64,
        quantity: Decimal,
        underlying_price: f64,
        profit: f64,
    ) -> Self {
        let mut event =
            OrderEvent::new(-1, symbol, NanosecondTimestamp(utc_ns), OrderStatus::Filled);
        event.id = -1;
        event.direction = OrderDirection::Buy;
        event.fill_quantity = quantity;
        event.quantity = quantity;
        event.message = format!("OTM. Underlying: {underlying_price:.4}. Profit: +{profit:.2}");
        Self { event }
    }

    /// Synthetic assignment/expiry event for `on_assignment_order_event`.
    pub fn expiry(symbol: Symbol, utc_ns: i64, quantity: Decimal, is_assignment: bool) -> Self {
        let mut event =
            OrderEvent::new(-1, symbol, NanosecondTimestamp(utc_ns), OrderStatus::Filled);
        event.id = -1;
        event.direction = OrderDirection::Hold;
        event.fill_quantity = quantity;
        event.quantity = quantity;
        event.is_assignment = is_assignment;
        event.is_in_the_money = is_assignment;
        event.message = if is_assignment {
            "Assignment"
        } else {
            "Expiry"
        }
        .to_string();
        Self { event }
    }
    pub fn order_id(&self) -> i64 {
        self.event.order_id
    }
    pub fn id(&self) -> i64 {
        self.event.id
    }
    pub fn symbol(&self) -> &Symbol {
        &self.event.symbol
    }
    pub fn utc_time_ns(&self) -> i64 {
        self.event.utc_time.0
    }
    pub fn status_code(&self) -> i64 {
        order_status_code(self.event.status)
    }
    pub fn direction_code(&self) -> i64 {
        order_direction_code(self.event.direction)
    }
    pub fn fill_price(&self) -> f64 {
        self.event.fill_price.to_f64().unwrap_or(0.0)
    }
    pub fn fill_price_currency(&self) -> &str {
        &self.event.fill_price_currency
    }
    pub fn fill_quantity(&self) -> f64 {
        self.event.fill_quantity.to_f64().unwrap_or(0.0)
    }
    pub fn absolute_fill_quantity(&self) -> f64 {
        self.fill_quantity().abs()
    }
    pub fn quantity(&self) -> f64 {
        self.event.quantity.to_f64().unwrap_or(0.0)
    }
    pub fn is_assignment(&self) -> bool {
        self.event.is_assignment
    }
    pub fn is_in_the_money(&self) -> bool {
        self.event.is_in_the_money
    }
    pub fn message(&self) -> &str {
        &self.event.message
    }
    pub fn is_fill(&self) -> bool {
        self.event.is_fill()
    }
    pub fn order_fee(&self) -> f64 {
        self.event.order_fee.to_f64().unwrap_or(0.0)
    }
    pub fn limit_price(&self) -> Option<f64> {
        self.event.limit_price.and_then(|p| p.to_f64())
    }
    pub fn stop_price(&self) -> Option<f64> {
        self.event.stop_price.and_then(|p| p.to_f64())
    }
    pub fn trigger_price(&self) -> Option<f64> {
        self.event.trigger_price.and_then(|p| p.to_f64())
    }
    pub fn trailing_amount(&self) -> Option<f64> {
        self.event.trailing_amount.and_then(|p| p.to_f64())
    }
    pub fn trailing_as_percentage(&self) -> bool {
        self.event.trailing_as_percentage
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl OrderEventView {
    #[getter(order_id)]
    fn py_order_id(&self) -> i64 {
        self.order_id()
    }

    #[getter(id)]
    fn py_id(&self) -> i64 {
        self.id()
    }

    #[getter(symbol)]
    fn py_symbol(&self) -> crate::securities::SymbolHandle {
        crate::securities::SymbolHandle::new(self.event.symbol.clone())
    }

    #[getter(utc_time)]
    fn py_utc_time(&self) -> chrono::NaiveDateTime {
        lean_core::NanosecondTimestamp(self.utc_time_ns())
            .to_utc()
            .naive_utc()
    }

    #[getter(status)]
    fn py_status(&self) -> crate::types::OrderStatus {
        use lean_orders::OrderStatus as Core;
        match self.event.status {
            Core::New => crate::types::OrderStatus::New,
            Core::Submitted => crate::types::OrderStatus::Submitted,
            Core::PartiallyFilled => crate::types::OrderStatus::PartiallyFilled,
            Core::Filled => crate::types::OrderStatus::Filled,
            Core::Canceled => crate::types::OrderStatus::Canceled,
            Core::None | Core::Invalid => crate::types::OrderStatus::Invalid,
            Core::CancelPending => crate::types::OrderStatus::CancelPending,
            Core::UpdateSubmitted => crate::types::OrderStatus::UpdateSubmitted,
        }
    }

    #[getter(direction)]
    fn py_direction(&self) -> crate::types::OrderDirection {
        use lean_orders::OrderDirection as Core;
        match self.event.direction {
            Core::Buy => crate::types::OrderDirection::Buy,
            Core::Sell => crate::types::OrderDirection::Sell,
            Core::Hold => crate::types::OrderDirection::Hold,
        }
    }

    #[getter(fill_price)]
    fn py_fill_price(&self) -> f64 {
        self.fill_price()
    }

    #[getter(fill_price_currency)]
    fn py_fill_price_currency(&self) -> String {
        self.fill_price_currency().to_string()
    }

    #[getter(fill_quantity)]
    fn py_fill_quantity(&self) -> f64 {
        self.fill_quantity()
    }

    #[getter(absolute_fill_quantity)]
    fn py_absolute_fill_quantity(&self) -> f64 {
        self.absolute_fill_quantity()
    }

    #[getter(quantity)]
    fn py_quantity(&self) -> f64 {
        self.quantity()
    }

    #[getter(is_assignment)]
    fn py_is_assignment(&self) -> bool {
        self.is_assignment()
    }

    #[getter(is_in_the_money)]
    fn py_is_in_the_money(&self) -> bool {
        self.is_in_the_money()
    }

    #[getter(message)]
    fn py_message(&self) -> String {
        self.message().to_string()
    }

    #[getter(order_fee)]
    fn py_order_fee(&self) -> f64 {
        self.order_fee()
    }

    #[getter(limit_price)]
    fn py_limit_price(&self) -> Option<f64> {
        self.limit_price()
    }

    #[getter(stop_price)]
    fn py_stop_price(&self) -> Option<f64> {
        self.stop_price()
    }

    #[getter(trigger_price)]
    fn py_trigger_price(&self) -> Option<f64> {
        self.trigger_price()
    }

    #[getter(trailing_amount)]
    fn py_trailing_amount(&self) -> Option<f64> {
        self.trailing_amount()
    }

    #[getter(trailing_as_percentage)]
    fn py_trailing_as_percentage(&self) -> bool {
        self.trailing_as_percentage()
    }

    #[pyo3(name = "is_fill")]
    fn py_is_fill(&self) -> bool {
        self.is_fill()
    }
}

/// SDK-owned projection of an `OrderTicket` for language bindings.
#[derive(Debug, Clone)]
pub struct OrderTicketView {
    pub ticket: OrderTicket,
}

impl OrderTicketView {
    pub fn new(ticket: OrderTicket) -> Self {
        Self { ticket }
    }

    pub fn order_id(&self) -> i64 {
        self.ticket.order_id
    }
    pub fn symbol(&self) -> Symbol {
        self.ticket.symbol.clone()
    }
    pub fn status_code(&self) -> i64 {
        order_status_code(self.ticket.status())
    }
    pub fn quantity(&self) -> f64 {
        self.ticket.quantity().to_f64().unwrap_or(0.0)
    }
    pub fn filled_quantity(&self) -> f64 {
        self.ticket.filled_quantity().to_f64().unwrap_or(0.0)
    }
    pub fn average_fill_price(&self) -> f64 {
        self.ticket.average_fill_price().to_f64().unwrap_or(0.0)
    }
    pub fn limit_price(&self) -> Option<f64> {
        self.ticket.order().limit_price.and_then(|p| p.to_f64())
    }
    pub fn stop_price(&self) -> Option<f64> {
        self.ticket.order().stop_price.and_then(|p| p.to_f64())
    }
    pub fn tag(&self) -> String {
        self.ticket.order().tag
    }
    pub fn is_open(&self) -> bool {
        self.ticket.is_open()
    }
    pub fn order_events(&self) -> Vec<OrderEventView> {
        self.ticket
            .order_events()
            .into_iter()
            .map(OrderEventView::new)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, NanosecondTimestamp, Symbol};
    use lean_orders::transaction_manager::TransactionManager;
    use rust_decimal_macros::dec;

    fn make_filled_event() -> OrderEvent {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut ev = OrderEvent::filled(
            42,
            sym,
            NanosecondTimestamp(1_620_000_000_000_000_000),
            dec!(420.00),
            dec!(10),
        );
        ev.id = 1;
        ev.order_fee = dec!(1.00);
        ev
    }

    #[test]
    fn order_event_view_is_fill() {
        let view = OrderEventView::new(make_filled_event());
        assert!(view.is_fill(), "Filled event must have is_fill == true");
    }

    #[test]
    fn order_event_view_fields() {
        let view = OrderEventView::new(make_filled_event());
        assert_eq!(view.order_id(), 42);
        assert_eq!(view.id(), 1);
        assert!((view.fill_price() - 420.0).abs() < 1e-9);
        assert!((view.fill_quantity() - 10.0).abs() < 1e-9);
        assert!((view.absolute_fill_quantity() - 10.0).abs() < 1e-9);
        assert!((view.quantity() - 10.0).abs() < 1e-9);
        assert!((view.order_fee() - 1.0).abs() < 1e-9);
        assert_eq!(view.fill_price_currency(), "USD");
        assert_eq!(view.message(), "Order filled");
        assert!(!view.is_assignment());
        assert!(!view.is_in_the_money());
        assert!(view.limit_price().is_none());
        assert!(view.stop_price().is_none());
    }

    #[test]
    fn order_event_view_not_fill_when_submitted() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let ev = OrderEvent::new(1, sym, NanosecondTimestamp(0), OrderStatus::Submitted);
        let view = OrderEventView::new(ev);
        assert!(!view.is_fill(), "Submitted event must not be a fill");
    }

    #[test]
    fn order_event_view_partial_fill() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut ev = OrderEvent::new(2, sym, NanosecondTimestamp(0), OrderStatus::PartiallyFilled);
        ev.fill_quantity = dec!(5);
        let view = OrderEventView::new(ev);
        assert!(
            view.is_fill(),
            "PartiallyFilled event must have is_fill == true"
        );
        assert!((view.absolute_fill_quantity() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn absolute_fill_quantity_negative_qty() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut ev = OrderEvent::filled(3, sym, NanosecondTimestamp(0), dec!(100), dec!(-5));
        ev.fill_quantity = dec!(-5);
        let view = OrderEventView::new(ev);
        assert!((view.fill_quantity() - (-5.0)).abs() < 1e-9);
        assert!(
            (view.absolute_fill_quantity() - 5.0).abs() < 1e-9,
            "absolute_fill_quantity must be positive for sell orders"
        );
    }

    #[test]
    fn otm_expiry_event_matches_lean_format() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let view =
            OrderEventView::otm_expiry(sym, 1_620_000_000_000_000_000, dec!(-1), 411.1234, 250.0);
        assert_eq!(view.fill_price(), 0.0);
        assert!(!view.is_assignment());
        assert_eq!(view.message(), "OTM. Underlying: 411.1234. Profit: +250.00");
        assert!(view.is_fill());
    }

    #[test]
    fn expiry_event_assignment_vs_expiry() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let assigned = OrderEventView::expiry(sym.clone(), 0, dec!(1), true);
        assert!(assigned.is_assignment());
        assert!(assigned.is_in_the_money());
        assert_eq!(assigned.message(), "Assignment");
        let expired = OrderEventView::expiry(sym, 0, dec!(1), false);
        assert!(!expired.is_assignment());
        assert_eq!(expired.message(), "Expiry");
    }

    #[test]
    fn order_ticket_transactions_update_from_primitive_request() {
        let manager = TransactionManager::new();
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut order = Order::limit(7, sym, dec!(10), dec!(100), DateTime::EPOCH, "old");
        order.status = OrderStatus::Submitted;
        manager.add_order(order);

        assert!(manager.update_order_ticket(
            7,
            DateTime::EPOCH,
            OrderTicketUpdateRequest::new(Some(101.25), None, Some("new".to_string())),
        ));

        let view = manager.order_ticket_view(7).expect("ticket view");
        assert_eq!(
            view.status_code(),
            order_status_code(OrderStatus::UpdateSubmitted)
        );
        assert_eq!(view.limit_price(), Some(101.25));
        assert_eq!(view.tag(), "new");
    }

    #[test]
    fn order_ticket_transactions_cancel_uses_optional_tag() {
        let manager = TransactionManager::new();
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut order = Order::market(8, sym, dec!(10), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        manager.add_order(order);

        assert!(manager.cancel_order_ticket(8, DateTime::EPOCH, Some("stop".to_string())));
        let requests = manager.get_cancel_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].tag, "stop");
    }

    #[test]
    fn order_ticket_handle_cancels_with_context_time() {
        let manager = Arc::new(TransactionManager::new());
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut order = Order::market(9, sym, dec!(10), DateTime::EPOCH, "");
        order.status = OrderStatus::Submitted;
        let ticket = manager.add_order(order);
        let cancel_time = NanosecondTimestamp(123_000_000_000);
        let handle = OrderTicketHandle::from_ticket(
            ticket,
            OrderTicketContext::fixed(manager.clone(), cancel_time),
        );

        assert!(handle.cancel(Some("risk".to_string())));
        assert_eq!(
            handle.status_code(),
            order_status_code(OrderStatus::CancelPending)
        );
        let requests = manager.get_cancel_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].time, cancel_time);
        assert_eq!(requests[0].tag, "risk");
    }

    #[test]
    fn order_ticket_handle_updates_limit_stop_and_tag() {
        let manager = Arc::new(TransactionManager::new());
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut order = Order::limit(10, sym, dec!(10), dec!(100), DateTime::EPOCH, "old");
        order.status = OrderStatus::Submitted;
        let ticket = manager.add_order(order);
        let update_time = NanosecondTimestamp(456_000_000_000);
        let handle = OrderTicketHandle::from_ticket(
            ticket,
            OrderTicketContext::fixed(manager.clone(), update_time),
        );

        assert!(handle.update(Some(101.5), Some(99.0), Some("new".to_string())));
        assert_eq!(handle.limit_price(), Some(101.5));
        assert_eq!(handle.stop_price(), Some(99.0));
        assert_eq!(handle.tag(), "new");
        let requests = manager.get_update_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].time, update_time);
    }

    #[test]
    fn order_ticket_invalid_update_and_cancel_ids_return_false() {
        // Mirrors LEAN OrderTicketTests invalid update/cancel ID cases at the
        // SDK transaction boundary.
        let manager = TransactionManager::new();

        assert!(!manager.update_order_ticket(
            404,
            DateTime::EPOCH,
            OrderTicketUpdateRequest::new(Some(101.0), None, Some("missing".to_string())),
        ));
        assert!(!manager.cancel_order_ticket(404, DateTime::EPOCH, Some("missing".to_string())));
        assert!(manager.order_ticket_view(404).is_none());
        assert!(manager.get_update_requests().is_empty());
        assert!(manager.get_cancel_requests().is_empty());
    }

    #[test]
    fn order_ticket_view_tracks_partial_and_final_fill_events() {
        // Mirrors LEAN OrderTicketTests/OrderEventTests coverage for ticket event
        // accumulation and average fill price after partial/final fills.
        let manager = TransactionManager::new();
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut order = Order::market(11, sym.clone(), dec!(10), DateTime::EPOCH, "entry");
        order.status = OrderStatus::Submitted;
        manager.add_order(order);

        manager.process_order_event(OrderEvent::new(
            11,
            sym.clone(),
            NanosecondTimestamp(1),
            OrderStatus::Submitted,
        ));
        let mut partial = OrderEvent::new(
            11,
            sym.clone(),
            NanosecondTimestamp(2),
            OrderStatus::PartiallyFilled,
        );
        partial.fill_price = dec!(100);
        partial.fill_quantity = dec!(4);
        manager.process_order_event(partial);
        let final_fill = OrderEvent::filled(11, sym, NanosecondTimestamp(3), dec!(110), dec!(6));
        manager.process_order_event(final_fill);

        let view = manager.order_ticket_view(11).expect("ticket view");
        assert_eq!(view.status_code(), order_status_code(OrderStatus::Filled));
        assert_eq!(view.order_events().len(), 3);
        assert!((view.filled_quantity() - 10.0).abs() < 1e-9);
        assert!((view.average_fill_price() - 106.0).abs() < 1e-9);
    }
}
