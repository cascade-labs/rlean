use crate::order::OrderStatus;
use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Emitted whenever an order's state changes.
///
/// Field names mirror C# LEAN's `OrderEvent` (snake_case). All optional price
/// fields (`limit_price`, `stop_price`, `trigger_price`, `trailing_amount`) are
/// `None` when not applicable to the order type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderEvent {
    /// Sequential event id (unique per order).
    pub id: i64,
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
    /// Commission and fees charged by the brokerage for this fill.
    pub order_fee: Price,
    /// Limit price for limit/stop-limit orders; `None` otherwise.
    pub limit_price: Option<Price>,
    /// Stop trigger price for stop/stop-limit orders; `None` otherwise.
    pub stop_price: Option<Price>,
    /// Trigger price for limit-if-touched orders; `None` otherwise.
    pub trigger_price: Option<Price>,
    /// Trailing amount for trailing-stop orders; `None` otherwise.
    pub trailing_amount: Option<Price>,
    /// When `true`, `trailing_amount` is expressed as a percentage (0–1).
    pub trailing_as_percentage: bool,
}

impl OrderEvent {
    pub fn new(order_id: i64, symbol: Symbol, time: DateTime, status: OrderStatus) -> Self {
        OrderEvent {
            id: 0,
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
            order_fee: dec!(0),
            limit_price: None,
            stop_price: None,
            trigger_price: None,
            trailing_amount: None,
            trailing_as_percentage: false,
        }
    }

    pub fn filled(order_id: i64, symbol: Symbol, time: DateTime, fill_price: Price, fill_quantity: Quantity) -> Self {
        let direction = crate::order::OrderDirection::from_quantity(fill_quantity);
        OrderEvent {
            id: 0,
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
            order_fee: dec!(0),
            limit_price: None,
            stop_price: None,
            trigger_price: None,
            trailing_amount: None,
            trailing_as_percentage: false,
        }
    }

    pub fn is_fill(&self) -> bool {
        matches!(self.status, OrderStatus::Filled | OrderStatus::PartiallyFilled)
    }
}
