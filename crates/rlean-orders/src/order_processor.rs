use crate::{
    fill_model::FillModel,
    order::{Order, OrderType},
    order_event::OrderEvent,
    transaction_manager::TransactionManager,
};
use rlean_core::DateTime;
use rlean_data_tables::{QuoteBar, TradeBar};
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
        self.process_orders_with_quotes(bars, &std::collections::HashMap::new(), time)
    }

    /// Scan all open orders and attempt fills against trade and quote bars.
    pub fn process_orders_with_quotes(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        quote_bars: &std::collections::HashMap<u64, QuoteBar>,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        let events = self.generate_order_events_with_quotes(bars, quote_bars, time);
        for event in &events {
            self.transaction_manager.process_order_event(event.clone());
        }
        events
    }

    pub fn generate_order_events(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        self.generate_order_events_with_quotes(bars, &std::collections::HashMap::new(), time)
    }

    pub fn generate_post_algorithm_order_events(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        self.generate_post_algorithm_order_events_with_quotes(
            bars,
            &std::collections::HashMap::new(),
            time,
        )
    }

    pub fn generate_order_events_with_quotes(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        quote_bars: &std::collections::HashMap<u64, QuoteBar>,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        self.generate_order_events_with_quotes_matching(bars, quote_bars, time, |_| true)
    }

    pub fn generate_post_algorithm_order_events_with_quotes(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        quote_bars: &std::collections::HashMap<u64, QuoteBar>,
        time: DateTime,
    ) -> Vec<OrderEvent> {
        self.generate_order_events_with_quotes_matching(bars, quote_bars, time, |order| {
            order.order_type == OrderType::Market
                || (order.time < time
                    && order
                        .last_update_time
                        .is_none_or(|last_update_time| last_update_time < time))
        })
    }

    fn generate_order_events_with_quotes_matching(
        &self,
        bars: &std::collections::HashMap<u64, TradeBar>,
        quote_bars: &std::collections::HashMap<u64, QuoteBar>,
        time: DateTime,
        predicate: impl Fn(&Order) -> bool,
    ) -> Vec<OrderEvent> {
        let open = self.transaction_manager.get_open_orders();
        let mut events = Vec::new();

        for order in open {
            if !predicate(&order) {
                continue;
            }
            let sid = order.symbol.id.sid;
            let event = match (bars.get(&sid), quote_bars.get(&sid)) {
                (Some(bar), quote_bar) => self.try_fill(&order, bar, quote_bar, time),
                (None, Some(quote_bar)) => self.try_fill_from_quote(&order, quote_bar, time),
                (None, None) => None,
            };
            if let Some(event) = event {
                events.push(event);
            }
        }

        events
    }

    fn try_fill(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Option<OrderEvent> {
        match order.order_type {
            OrderType::Market => {
                let fill = self
                    .fill_model
                    .market_fill_with_quotes(order, bar, quote_bar, time);
                Some(fill.order_event)
            }
            OrderType::Limit => self
                .fill_model
                .limit_fill_with_quotes(order, bar, quote_bar, time)
                .map(|f| f.order_event),
            OrderType::StopMarket => self
                .fill_model
                .stop_market_fill_with_quotes(order, bar, quote_bar, time)
                .map(|f| f.order_event),
            OrderType::StopLimit => self
                .fill_model
                .stop_limit_fill_with_quotes(order, bar, quote_bar, time)
                .map(|f| f.order_event),
            OrderType::MarketOnOpen => {
                let fill = self
                    .fill_model
                    .market_on_open_fill_with_quotes(order, bar, quote_bar, time);
                Some(fill.order_event)
            }
            OrderType::MarketOnClose => {
                let fill = self
                    .fill_model
                    .market_on_close_fill_with_quotes(order, bar, quote_bar, time);
                Some(fill.order_event)
            }
            _ => None,
        }
    }

    fn try_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<OrderEvent> {
        match order.order_type {
            OrderType::Market => self
                .fill_model
                .market_fill_from_quote(order, quote_bar, time)
                .map(|fill| fill.order_event),
            OrderType::Limit => self
                .fill_model
                .limit_fill_from_quote(order, quote_bar, time)
                .map(|fill| fill.order_event),
            OrderType::StopMarket => self
                .fill_model
                .stop_market_fill_from_quote(order, quote_bar, time)
                .map(|fill| fill.order_event),
            OrderType::StopLimit => self
                .fill_model
                .stop_limit_fill_from_quote(order, quote_bar, time)
                .map(|fill| fill.order_event),
            OrderType::MarketOnOpen => self
                .fill_model
                .market_on_open_fill_from_quote(order, quote_bar, time)
                .map(|fill| fill.order_event),
            OrderType::MarketOnClose => self
                .fill_model
                .market_on_close_fill_from_quote(order, quote_bar, time)
                .map(|fill| fill.order_event),
            _ => None,
        }
    }
}
