use crate::order::{Order, OrderStatus};
use crate::order_event::OrderEvent;
use lean_core::{DateTime, Price, Quantity, Symbol};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateOrderFields {
    pub quantity: Option<Quantity>,
    pub limit_price: Option<Price>,
    pub stop_price: Option<Price>,
    pub tag: Option<String>,
}

/// Handle to a submitted order. Returned to the algorithm on every order() call.
#[derive(Debug, Clone)]
pub struct OrderTicket {
    pub order_id: i64,
    pub symbol: Symbol,
    inner: Arc<RwLock<OrderTicketInner>>,
}

#[derive(Debug)]
struct OrderTicketInner {
    pub order: Order,
    pub order_events: Vec<OrderEvent>,
}

impl OrderTicket {
    pub fn new(order: Order) -> Self {
        let symbol = order.symbol.clone();
        let id = order.id;
        OrderTicket {
            order_id: id,
            symbol,
            inner: Arc::new(RwLock::new(OrderTicketInner {
                order,
                order_events: vec![],
            })),
        }
    }

    pub fn status(&self) -> OrderStatus {
        self.inner.read().order.status
    }

    pub fn quantity(&self) -> Quantity {
        self.inner.read().order.quantity
    }

    pub fn average_fill_price(&self) -> Price {
        self.inner.read().order.average_fill_price
    }

    pub fn filled_quantity(&self) -> Quantity {
        self.inner.read().order.filled_quantity
    }

    pub fn is_open(&self) -> bool {
        self.status().is_open()
    }

    pub fn order_events(&self) -> Vec<OrderEvent> {
        self.inner.read().order_events.clone()
    }

    pub fn add_order_event(&self, event: OrderEvent) {
        let mut inner = self.inner.write();
        inner.order.status = event.status;
        if event.is_fill() {
            inner.order.last_fill_time = Some(event.utc_time);
            inner.order.filled_quantity += event.fill_quantity;
            // Update VWAP
            let prev_filled = inner.order.filled_quantity - event.fill_quantity;
            let prev_value = inner.order.average_fill_price * prev_filled;
            let new_value = event.fill_price * event.fill_quantity;
            if inner.order.filled_quantity != lean_core::Price::ZERO {
                inner.order.average_fill_price =
                    (prev_value + new_value) / inner.order.filled_quantity;
            }
        }
        inner.order_events.push(event);
    }

    pub fn cancel(&self, cancel_time: DateTime) {
        let mut inner = self.inner.write();
        inner.order.status = OrderStatus::Canceled;
        inner.order.canceled_time = Some(cancel_time);
    }
}
