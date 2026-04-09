use crate::order::OrderStatus;
use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Emitted whenever an order's state changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderEvent {
    pub order_id: i64,
    pub symbol: Symbol,
    pub utc_time: DateTime,
    pub status: OrderStatus,
    pub direction: crate::order::OrderDirection,
    pub fill_price: Price,
    pub fill_price_currency: String,
    pub fill_quantity: Quantity,
    pub is_assignment: bool,
    pub is_in_the_money: bool,
    pub quantity: Quantity,
    pub message: String,
    pub shortable_inventory: Option<Quantity>,
}

impl OrderEvent {
    pub fn new(order_id: i64, symbol: Symbol, time: DateTime, status: OrderStatus) -> Self {
        OrderEvent {
            order_id,
            symbol,
            utc_time: time,
            status,
            direction: crate::order::OrderDirection::Hold,
            fill_price: dec!(0),
            fill_price_currency: "USD".into(),
            fill_quantity: dec!(0),
            is_assignment: false,
            is_in_the_money: false,
            quantity: dec!(0),
            message: String::new(),
            shortable_inventory: None,
        }
    }

    pub fn filled(order_id: i64, symbol: Symbol, time: DateTime, fill_price: Price, fill_quantity: Quantity) -> Self {
        let direction = crate::order::OrderDirection::from_quantity(fill_quantity);
        OrderEvent {
            order_id,
            symbol,
            utc_time: time,
            status: OrderStatus::Filled,
            direction,
            fill_price,
            fill_price_currency: "USD".into(),
            fill_quantity,
            is_assignment: false,
            is_in_the_money: false,
            quantity: fill_quantity,
            message: "Order filled".into(),
            shortable_inventory: None,
        }
    }

    pub fn is_fill(&self) -> bool {
        matches!(self.status, OrderStatus::Filled | OrderStatus::PartiallyFilled)
    }
}
