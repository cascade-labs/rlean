use rlean_core::{DateTime, Price, Result as LeanResult, Symbol};
use rlean_orders::{Order, UpdateOrderRequest};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct BrokerageHolding {
    pub symbol: Symbol,
    pub quantity: rlean_core::Quantity,
    pub average_price: Price,
    pub market_price: Price,
}

#[derive(Debug, Clone)]
pub struct BrokerageTransaction {
    pub id: String,
    pub order_id: i64,
    pub fill_price: Price,
    pub fill_quantity: rlean_core::Quantity,
    pub time: DateTime,
    pub commission: Price,
}

/// Interface that every brokerage (paper, IB, Alpaca, etc.) implements.
pub trait Brokerage: Send + Sync {
    fn name(&self) -> &str;
    /// LEAN brokerage model advertised by this execution adapter. Transport
    /// adapters use this to keep order and buying-power semantics independent
    /// from the URL or process serving the brokerage API.
    fn brokerage_model(&self) -> Option<&str> {
        None
    }
    fn is_connected(&self) -> bool;
    /// Returns true when the brokerage adapter should be exercised for
    /// order validation/state, but fills should be generated locally.
    fn uses_local_paper_fills(&self) -> bool {
        false
    }
    fn connect(&mut self) -> LeanResult<()>;
    fn disconnect(&mut self);
    fn place_order(&mut self, order: Order) -> LeanResult<bool>;
    fn place_order_with_brokerage_ids(&mut self, order: Order) -> LeanResult<Option<Vec<String>>> {
        if self.place_order(order)? {
            Ok(Some(Vec::new()))
        } else {
            Ok(None)
        }
    }
    fn update_order(&mut self, order: &Order) -> LeanResult<bool>;
    fn can_update_order(&self, _order: &Order, _request: &UpdateOrderRequest) -> bool {
        true
    }
    fn cancel_order(&mut self, order: &Order) -> LeanResult<bool>;
    fn get_open_orders(&self) -> Vec<Order>;
    fn get_account_orders(&self) -> Vec<Order> {
        self.get_open_orders()
    }
    fn get_cash_balance(&self) -> Vec<(String, Price)>;
    fn get_account_holdings(&self) -> HashMap<Symbol, rlean_core::Quantity>;
    fn get_account_detailed_holdings(&self) -> Vec<BrokerageHolding> {
        self.get_account_holdings()
            .into_iter()
            .map(|(symbol, quantity)| BrokerageHolding {
                symbol,
                quantity,
                average_price: Price::ZERO,
                market_price: Price::ZERO,
            })
            .collect()
    }
}
