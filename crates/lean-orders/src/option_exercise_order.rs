use crate::order::{Order, OrderType};
use lean_core::{DateTime, Quantity, Symbol};

/// Order used to exercise or assign an option contract.
/// Generated automatically by the engine at expiration, or manually by the algorithm.
///
/// When processed, yields TWO fill events via DefaultExerciseModel:
///   1. Close the option position (fill price = 0)
///   2. Create/close the underlying position (fill price = strike price)
#[derive(Debug, Clone)]
pub struct OptionExerciseOrder {
    pub order: Order,
}

impl OptionExerciseOrder {
    pub fn new(id: i64, symbol: Symbol, quantity: Quantity, time: DateTime, tag: &str) -> Self {
        let mut order = Order::market(id, symbol, quantity, time, tag);
        order.order_type = OrderType::OptionExercise;
        OptionExerciseOrder { order }
    }
}
