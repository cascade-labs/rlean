use crate::brokerage::Brokerage;
use dashmap::DashMap;
use lean_algorithm::portfolio::SecurityPortfolioManager;
use lean_core::{Price, Quantity, Result as LeanResult, Symbol};
use lean_orders::{Order, OrderStatus};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Paper trading brokerage — simulates order execution with no real capital.
pub struct PaperBrokerage {
    name: String,
    connected: bool,
    orders: Arc<DashMap<i64, Order>>,
    cash: Arc<Mutex<Price>>,
    portfolio: Arc<SecurityPortfolioManager>,
}

impl PaperBrokerage {
    pub fn new(starting_cash: Price, portfolio: Arc<SecurityPortfolioManager>) -> Self {
        PaperBrokerage {
            name: "Paper".to_string(),
            connected: false,
            orders: Arc::new(DashMap::new()),
            cash: Arc::new(Mutex::new(starting_cash)),
            portfolio,
        }
    }

    pub fn cash_balance(&self) -> Price {
        *self.cash.lock()
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
        self.orders.insert(order.id, order);
        Ok(true)
    }

    fn update_order(&mut self, order: &Order) -> LeanResult<bool> {
        if let Some(mut entry) = self.orders.get_mut(&order.id) {
            *entry = order.clone();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn cancel_order(&mut self, order: &Order) -> LeanResult<bool> {
        if let Some(mut entry) = self.orders.get_mut(&order.id) {
            entry.status = OrderStatus::Canceled;
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
        vec![("USD".to_string(), *self.cash.lock())]
    }

    fn get_account_holdings(&self) -> HashMap<Symbol, Quantity> {
        self.portfolio
            .all_holdings()
            .into_iter()
            .filter(|h| h.is_invested())
            .map(|h| (h.symbol.clone(), h.quantity))
            .collect()
    }
}
