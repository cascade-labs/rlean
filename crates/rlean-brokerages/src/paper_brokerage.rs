use crate::brokerage::{Brokerage, BrokerageHolding};
use dashmap::DashMap;
use parking_lot::Mutex;
use rlean_algorithm::portfolio::SecurityPortfolioManager;
use rlean_core::{Price, Quantity, Result as LeanResult, Symbol};
use rlean_data::Slice;
use rlean_orders::{
    FillModel, ImmediateFillModel, NullSlippageModel, Order, OrderEvent, OrderProcessor,
    OrderStatus, TransactionManager,
};
use std::collections::HashMap;
use std::sync::Arc;

/// Paper trading brokerage — simulates order execution with no real capital.
pub struct PaperBrokerage {
    name: String,
    connected: bool,
    orders: Arc<DashMap<i64, Order>>,
    cash: Arc<Mutex<Price>>,
    portfolio: Arc<SecurityPortfolioManager>,
    transactions: Arc<TransactionManager>,
    order_processor: OrderProcessor,
    order_events: Arc<Mutex<Vec<OrderEvent>>>,
}

impl PaperBrokerage {
    pub fn new(starting_cash: Price, portfolio: Arc<SecurityPortfolioManager>) -> Self {
        let transactions = Arc::new(TransactionManager::new());
        Self::new_with_transactions(starting_cash, portfolio, transactions)
    }

    pub fn new_with_transactions(
        starting_cash: Price,
        portfolio: Arc<SecurityPortfolioManager>,
        transactions: Arc<TransactionManager>,
    ) -> Self {
        let fill_model: Box<dyn FillModel> =
            Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel)));
        let order_processor = OrderProcessor::new(fill_model, transactions.clone());
        PaperBrokerage {
            name: "Paper".to_string(),
            connected: false,
            orders: Arc::new(DashMap::new()),
            cash: Arc::new(Mutex::new(starting_cash)),
            portfolio,
            transactions,
            order_processor,
            order_events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn cash_balance(&self) -> Price {
        *self.portfolio.cash.read()
    }

    pub fn transaction_manager(&self) -> Arc<TransactionManager> {
        self.transactions.clone()
    }

    pub fn order_events(&self) -> Vec<OrderEvent> {
        self.order_events.lock().clone()
    }

    pub fn scan(&mut self, slice: &Slice) -> LeanResult<Vec<OrderEvent>> {
        let events = self.order_processor.process_orders_with_quotes(
            &slice.bars,
            &slice.quote_bars,
            slice.time,
        );

        for event in &events {
            if let Some(order) = self.transactions.get_order(event.order_id) {
                self.orders.insert(order.id, order.clone());
                if event.is_fill() && !event.fill_quantity.is_zero() {
                    self.portfolio.apply_fill(
                        &order,
                        event.fill_price,
                        event.fill_quantity,
                        event.order_fee,
                    );
                }
            }
        }

        self.order_events.lock().extend(events.iter().cloned());
        *self.cash.lock() = *self.portfolio.cash.read();
        Ok(events)
    }

    pub fn scan_order_events(&self, slice: &Slice) -> Vec<OrderEvent> {
        self.order_processor.generate_order_events_with_quotes(
            &slice.bars,
            &slice.quote_bars,
            slice.time,
        )
    }

    fn record_event(&self, event: OrderEvent) {
        self.transactions.process_order_event(event.clone());
        self.order_events.lock().push(event);
    }

    fn order_event(
        &self,
        order: &Order,
        status: OrderStatus,
        message: impl Into<String>,
    ) -> OrderEvent {
        let mut event = OrderEvent::new(order.id, order.symbol.clone(), order.time, status);
        event.apply_order_fields(order);
        event.message = message.into();
        event
    }
}

impl Brokerage for PaperBrokerage {
    fn name(&self) -> &str {
        &self.name
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
        let mut order = order;
        order.status = OrderStatus::Submitted;
        self.orders.insert(order.id, order.clone());
        self.transactions.add_or_update_order(order.clone());

        let event = self.order_event(&order, OrderStatus::Submitted, "Order submitted");
        self.record_event(event);
        Ok(true)
    }

    fn update_order(&mut self, order: &Order) -> LeanResult<bool> {
        if let Some(mut entry) = self.orders.get_mut(&order.id) {
            let mut updated = order.clone();
            updated.status = OrderStatus::UpdateSubmitted;
            updated.last_update_time = Some(order.time);
            *entry = updated.clone();
            self.transactions.update_order(updated.clone());

            let event = self.order_event(
                &updated,
                OrderStatus::UpdateSubmitted,
                "Order update submitted",
            );
            self.record_event(event);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn cancel_order(&mut self, order: &Order) -> LeanResult<bool> {
        if let Some(mut entry) = self.orders.get_mut(&order.id) {
            entry.status = OrderStatus::Canceled;
            entry.canceled_time = Some(order.time);
            self.transactions.update_order(entry.clone());

            let event = self.order_event(order, OrderStatus::Canceled, "Order canceled");
            self.record_event(event);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn get_open_orders(&self) -> Vec<Order> {
        self.orders
            .iter()
            .filter(|e| e.is_open())
            .map(|e| e.clone())
            .collect()
    }

    fn get_cash_balance(&self) -> Vec<(String, Price)> {
        vec![("USD".to_string(), self.cash_balance())]
    }

    fn get_account_holdings(&self) -> HashMap<Symbol, Quantity> {
        self.portfolio
            .all_holdings()
            .into_iter()
            .filter(|h| h.is_invested())
            .map(|h| (h.symbol.clone(), h.quantity))
            .collect()
    }

    fn get_account_detailed_holdings(&self) -> Vec<BrokerageHolding> {
        self.portfolio
            .all_holdings()
            .into_iter()
            .filter(|h| h.is_invested())
            .map(|h| BrokerageHolding {
                symbol: h.symbol,
                quantity: h.quantity,
                average_price: h.average_price,
                market_price: h.last_price,
            })
            .collect()
    }
}
