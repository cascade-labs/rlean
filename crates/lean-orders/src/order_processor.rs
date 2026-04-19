use crate::{
    fill_model::FillModel,
    order::{Order, OrderType},
    order_event::OrderEvent,
    transaction_manager::TransactionManager,
};
use lean_core::DateTime;
use lean_data::TradeBar;
use std::sync::Arc;

/// Processes pending orders against current market data.
pub struct OrderProcessor {
    pub fill_model: Box<dyn FillModel>,
    pub transaction_manager: Arc<TransactionManager>,
}

impl OrderProcessor {
    pub fn new(fill_model: Box<dyn FillModel>, tm: Arc<TransactionManager>) -> Self {
        OrderProcessor {
            fill_model,
            transaction_manager: tm,
        }
    }

    /// Scan all open orders and attempt fills against `bars`.
    pub fn process_orders(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        let open = self.transaction_manager.get_open_orders();
        let mut events = Vec::new();

        for order in open {
            let sid = order.symbol.id.sid;
            if let Some(bar) = bars.get(&sid) {
                if let Some(event) = self.try_fill(&order, bar, time) {
                    self.transaction_manager.process_order_event(event.clone());
                    events.push(event);
                }
            }
        }

        events
    }

    fn try_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<OrderEvent> {
        match order.order_type {
            OrderType::Market => {
                let fill = self.fill_model.market_fill(order, bar, time);
                Some(fill.order_event)
            }
            OrderType::Limit => self
                .fill_model
                .limit_fill(order, bar, time)
                .map(|f| f.order_event),
            OrderType::StopMarket => self
                .fill_model
                .stop_market_fill(order, bar, time)
                .map(|f| f.order_event),
            OrderType::StopLimit => self
                .fill_model
                .stop_limit_fill(order, bar, time)
                .map(|f| f.order_event),
            OrderType::MarketOnOpen => {
                let fill = self.fill_model.market_on_open_fill(order, bar, time);
                Some(fill.order_event)
            }
            OrderType::MarketOnClose => {
                let fill = self.fill_model.market_on_close_fill(order, bar, time);
                Some(fill.order_event)
            }
            _ => None,
        }
    }
}
