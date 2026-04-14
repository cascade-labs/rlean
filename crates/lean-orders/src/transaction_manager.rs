use crate::{order::Order, order_event::OrderEvent, order_ticket::OrderTicket};
use dashmap::DashMap;
use lean_core::DateTime;
use parking_lot::RwLock;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Thread-safe store for all orders in a strategy run.
pub struct TransactionManager {
    orders: DashMap<i64, Arc<RwLock<Order>>>,
    tickets: DashMap<i64, OrderTicket>,
    next_order_id: AtomicI64,
}

impl TransactionManager {
    pub fn new() -> Self {
        TransactionManager {
            orders: DashMap::new(),
            tickets: DashMap::new(),
            next_order_id: AtomicI64::new(1),
        }
    }

    pub fn next_order_id(&self) -> i64 {
        self.next_order_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn add_order(&self, order: Order) -> OrderTicket {
        let id = order.id;
        let ticket = OrderTicket::new(order.clone());
        self.orders.insert(id, Arc::new(RwLock::new(order)));
        self.tickets.insert(id, ticket.clone());
        ticket
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
            let new_status = event.status;
            order_lock.write().status = new_status;
        }
        if let Some(ticket) = self.tickets.get(&event.order_id) {
            ticket.add_order_event(event);
        }
    }

    pub fn get_open_orders(&self) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|entry| entry.read().is_open())
            .map(|entry| entry.read().clone())
            .collect()
    }

    pub fn get_orders_by_symbol(&self, symbol_sid: u64) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|entry| entry.read().symbol.id.sid == symbol_sid)
            .map(|entry| entry.read().clone())
            .collect()
    }

    pub fn cancel_open_orders(&self, time: DateTime) {
        for entry in self.orders.iter() {
            if entry.read().is_open() {
                if let Some(ticket) = self.tickets.get(&entry.read().id) {
                    ticket.cancel(time);
                }
            }
        }
    }

    pub fn order_count(&self) -> usize {
        self.orders.len()
    }

    pub fn get_all_orders(&self) -> Vec<Order> {
        self.orders.iter().map(|entry| entry.read().clone()).collect()
    }

    pub fn get_all_order_events(&self) -> Vec<OrderEvent> {
        let mut events: Vec<OrderEvent> = self.tickets
            .iter()
            .flat_map(|t| t.order_events())
            .collect();
        events.sort_by_key(|e| e.utc_time);
        events
    }
}

impl Default for TransactionManager {
    fn default() -> Self {
        TransactionManager::new()
    }
}
