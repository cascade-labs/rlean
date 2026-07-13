use crate::{
    order::{Order, OrderStatus},
    order_event::OrderEvent,
    order_ticket::{OrderTicket, UpdateOrderFields},
};
use dashmap::{DashMap, DashSet};
use parking_lot::RwLock;
use rlean_core::{DateTime, Price};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelOrderRequest {
    pub order_id: i64,
    pub time: DateTime,
    pub tag: String,
}

#[derive(Debug, Clone)]
pub struct UpdateOrderRequest {
    pub order_id: i64,
    pub time: DateTime,
    pub fields: UpdateOrderFields,
    pub previous_order: Order,
}

/// Thread-safe store for all orders in a strategy run.
pub struct TransactionManager {
    orders: DashMap<i64, Arc<RwLock<Order>>>,
    open_order_ids: DashSet<i64>,
    tickets: DashMap<i64, OrderTicket>,
    cancel_requests: DashMap<i64, CancelOrderRequest>,
    update_requests: DashMap<i64, UpdateOrderRequest>,
    next_order_id: AtomicI64,
    open_order_count: AtomicUsize,
}

impl TransactionManager {
    pub fn new() -> Self {
        TransactionManager {
            orders: DashMap::new(),
            open_order_ids: DashSet::new(),
            tickets: DashMap::new(),
            cancel_requests: DashMap::new(),
            update_requests: DashMap::new(),
            next_order_id: AtomicI64::new(1),
            open_order_count: AtomicUsize::new(0),
        }
    }

    pub fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn add_order(&self, order: Order) -> OrderTicket {
        let id = order.id;
        let ticket = OrderTicket::new(order.clone());
        if order.is_open() {
            self.open_order_ids.insert(id);
            self.open_order_count.fetch_add(1, Ordering::Relaxed);
        }
        self.orders.insert(id, Arc::new(RwLock::new(order)));
        self.tickets.insert(id, ticket.clone());
        ticket
    }

    pub fn add_or_update_order(&self, order: Order) -> OrderTicket {
        let id = order.id;
        if let Some(order_lock) = self.orders.get(&id) {
            let was_open = order_lock.read().is_open();
            let is_open = order.is_open();
            *order_lock.write() = order.clone();
            self.adjust_open_order_index(id, was_open, is_open);
            if let Some(ticket) = self.tickets.get(&id) {
                ticket.set_order(order);
                return ticket.clone();
            }
        }
        self.add_order(order)
    }

    pub fn update_order(&self, order: Order) -> bool {
        if let Some(order_lock) = self.orders.get(&order.id) {
            let was_open = order_lock.read().is_open();
            let is_open = order.is_open();
            *order_lock.write() = order.clone();
            self.adjust_open_order_index(order.id, was_open, is_open);
            if let Some(ticket) = self.tickets.get(&order.id) {
                ticket.set_order(order);
            }
            true
        } else {
            false
        }
    }

    pub fn get_order(&self, id: i64) -> Option<Order> {
        self.orders.get(&id).map(|o| o.read().clone())
    }

    pub fn get_ticket(&self, id: i64) -> Option<OrderTicket> {
        self.tickets.get(&id).map(|t| t.clone())
    }

    pub fn process_order_event(&self, event: OrderEvent) {
        // Update the order's status so it no longer appears in get_open_orders().
        if let Some(order_lock) = self.orders.get(&event.order_id) {
            let mut order = order_lock.write();
            let was_open = order.is_open();
            order.status = event.status;
            if event.status == OrderStatus::Canceled {
                order.canceled_time = Some(event.utc_time);
            }
            if event.is_fill() {
                order.last_fill_time = Some(event.utc_time);
                let previous_filled = order.filled_quantity;
                let new_filled = previous_filled + event.fill_quantity;
                let previous_value = order.average_fill_price * previous_filled;
                let new_value = event.fill_price * event.fill_quantity;
                if !new_filled.is_zero() {
                    order.average_fill_price = (previous_value + new_value) / new_filled;
                }
                order.filled_quantity = new_filled;
            }
            self.adjust_open_order_index(order.id, was_open, order.is_open());
        }
        if let Some(ticket) = self.tickets.get(&event.order_id) {
            ticket.add_order_event(event);
        }
    }

    pub fn get_open_orders(&self) -> Vec<Order> {
        if !self.has_open_orders() {
            return Vec::new();
        }
        self.open_order_ids
            .iter()
            .filter_map(|order_id| {
                self.orders
                    .get(order_id.key())
                    .map(|entry| entry.read().clone())
            })
            .collect()
    }

    pub fn has_open_orders(&self) -> bool {
        self.open_order_count.load(Ordering::Relaxed) != 0
    }

    /// Apply C# LEAN's default split order adjustment to open orders.
    ///
    /// Quantity is divided by the LEAN split factor; price fields are multiplied
    /// by it so the order retains approximately the same notional value.
    pub fn apply_split_to_open_orders(&self, symbol_sid: u64, split_factor: Price) {
        if split_factor.is_zero() {
            return;
        }
        for entry in self.orders.iter() {
            let mut order = entry.write();
            if order.symbol.id.sid != symbol_sid || !order.is_open() {
                continue;
            }

            order.quantity = (order.quantity / split_factor).trunc();
            order.price *= split_factor;
            if let Some(limit_price) = order.limit_price.as_mut() {
                *limit_price *= split_factor;
            }
            if let Some(stop_price) = order.stop_price.as_mut() {
                *stop_price *= split_factor;
            }
            let trailing_as_percent = order.trailing_as_percent;
            if let Some(trailing_amount) = order.trailing_amount.as_mut() {
                if !trailing_as_percent {
                    *trailing_amount *= split_factor;
                }
            }
        }
    }

    pub fn get_orders_by_symbol(&self, symbol_sid: u64) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|entry| entry.read().symbol.id.sid == symbol_sid)
            .map(|entry| entry.read().clone())
            .collect()
    }

    fn adjust_open_order_index(&self, order_id: i64, was_open: bool, is_open: bool) {
        match (was_open, is_open) {
            (false, true) => {
                self.open_order_ids.insert(order_id);
                self.open_order_count.fetch_add(1, Ordering::Relaxed);
            }
            (true, false) => {
                self.open_order_ids.remove(&order_id);
                self.open_order_count.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {}
        }
    }

    pub fn request_cancel_order(&self, order_id: i64, time: DateTime, tag: String) -> bool {
        let Some(order_lock) = self.orders.get(&order_id) else {
            return false;
        };

        {
            let mut order = order_lock.write();
            if !order.is_open() {
                return false;
            }
            let was_open = order.is_open();
            order.status = OrderStatus::CancelPending;
            order.canceled_time = None;
            self.adjust_open_order_index(order_id, was_open, order.is_open());
        }

        let event_symbol = order_lock.read().symbol.clone();
        if let Some(ticket) = self.tickets.get(&order_id) {
            ticket.add_order_event(OrderEvent::new(
                order_id,
                event_symbol,
                time,
                OrderStatus::CancelPending,
            ));
        }
        self.cancel_requests.insert(
            order_id,
            CancelOrderRequest {
                order_id,
                time,
                tag,
            },
        );
        self.update_requests.remove(&order_id);
        true
    }

    pub fn get_cancel_requests(&self) -> Vec<CancelOrderRequest> {
        self.cancel_requests
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn clear_cancel_request(&self, order_id: i64) {
        self.cancel_requests.remove(&order_id);
    }

    pub fn request_update_order(
        &self,
        order_id: i64,
        time: DateTime,
        fields: UpdateOrderFields,
    ) -> bool {
        let Some(order_lock) = self.orders.get(&order_id) else {
            return false;
        };

        let previous_order = order_lock.read().clone();
        if !previous_order.is_open() || previous_order.status == OrderStatus::CancelPending {
            return false;
        }

        let mut updated_order = apply_update_fields(previous_order.clone(), &fields);
        updated_order.last_update_time = Some(time);

        if previous_order.status == OrderStatus::New {
            updated_order.status = OrderStatus::New;
            return self.update_order(updated_order);
        }

        updated_order.status = OrderStatus::UpdateSubmitted;
        self.update_order(updated_order.clone());
        if let Some(ticket) = self.tickets.get(&order_id) {
            ticket.add_order_event(OrderEvent::new(
                order_id,
                updated_order.symbol.clone(),
                time,
                OrderStatus::UpdateSubmitted,
            ));
        }
        self.update_requests.insert(
            order_id,
            UpdateOrderRequest {
                order_id,
                time,
                fields,
                previous_order,
            },
        );
        true
    }

    pub fn get_update_requests(&self) -> Vec<UpdateOrderRequest> {
        self.update_requests
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    pub fn clear_update_request(&self, order_id: i64) {
        self.update_requests.remove(&order_id);
    }

    pub fn cancel_open_orders(&self, time: DateTime) {
        for order_id in self.open_order_ids.iter().map(|id| *id).collect::<Vec<_>>() {
            let Some(entry) = self.orders.get(&order_id) else {
                continue;
            };
            let mut order = entry.write();
            if order.is_open() {
                let was_open = order.is_open();
                order.status = OrderStatus::Canceled;
                order.canceled_time = Some(time);
                self.adjust_open_order_index(order.id, was_open, order.is_open());
                if let Some(ticket) = self.tickets.get(&order.id) {
                    ticket.cancel(time);
                }
            }
        }
    }

    pub fn cancel_open_orders_for_symbol(&self, symbol_sid: u64, time: DateTime) {
        for order_id in self.open_order_ids.iter().map(|id| *id).collect::<Vec<_>>() {
            let Some(entry) = self.orders.get(&order_id) else {
                continue;
            };
            let mut order = entry.write();
            if order.symbol.id.sid == symbol_sid && order.is_open() {
                let was_open = order.is_open();
                order.status = OrderStatus::Canceled;
                order.canceled_time = Some(time);
                self.adjust_open_order_index(order.id, was_open, order.is_open());
                if let Some(ticket) = self.tickets.get(&order.id) {
                    ticket.cancel(time);
                }
            }
        }
    }

    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    pub fn get_all_orders(&self) -> Vec<Order> {
        self.orders
            .iter()
            .map(|entry| entry.read().clone())
            .collect()
    }

    pub fn get_all_order_events(&self) -> Vec<OrderEvent> {
        let mut events: Vec<OrderEvent> =
            self.tickets.iter().flat_map(|t| t.order_events()).collect();
        events.sort_by_key(|e| e.utc_time);
        events
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        TransactionManager::new()
    }
}

fn apply_update_fields(mut order: Order, fields: &UpdateOrderFields) -> Order {
    if let Some(quantity) = fields.quantity {
        order.quantity = quantity;
    }
    if let Some(limit_price) = fields.limit_price {
        order.limit_price = Some(limit_price);
        if matches!(
            order.order_type,
            crate::order::OrderType::Limit | crate::order::OrderType::StopLimit
        ) {
            order.price = limit_price;
        }
    }
    if let Some(stop_price) = fields.stop_price {
        order.stop_price = Some(stop_price);
        if order.order_type == crate::order::OrderType::StopMarket {
            order.price = stop_price;
        }
    }
    if let Some(tag) = &fields.tag {
        order.tag = tag.clone();
    }
    order
}
