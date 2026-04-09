use lean_core::{DateTime, Price, Result as LeanResult, Symbol};
use lean_orders::{Order, OrderEvent, OrderTicket};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct BrokerageTransaction {
    pub id: String,
    pub order_id: i64,
    pub fill_price: Price,
    pub fill_quantity: lean_core::Quantity,
    pub time: DateTime,
    pub commission: Price,
}

/// Interface that every brokerage (paper, IB, Alpaca, etc.) implements.
pub trait Brokerage: Send + Sync {
    fn name(&self) -> &str;
    fn is_connected(&self) -> bool;
    fn connect(&mut self) -> LeanResult<()>;
    fn disconnect(&mut self);
    fn place_order(&mut self, order: Order) -> LeanResult<bool>;
    fn update_order(&mut self, order: &Order) -> LeanResult<bool>;
    fn cancel_order(&mut self, order: &Order) -> LeanResult<bool>;
    fn get_open_orders(&self) -> Vec<Order>;
    fn get_cash_balance(&self) -> Vec<(String, Price)>;
    fn get_account_holdings(&self) -> HashMap<Symbol, lean_core::Quantity>;
}
