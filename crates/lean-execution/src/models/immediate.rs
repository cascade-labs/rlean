use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use lean_core::Symbol;
use rust_decimal::Decimal;

/// Immediately submits market orders to achieve desired portfolio targets.
///
/// Mirrors C# ImmediateExecutionModel: computes delta between desired quantity and
/// current holdings, then fires a market order for that difference.
pub struct ImmediateExecutionModel;

impl ImmediateExecutionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ImmediateExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IExecutionModel for ImmediateExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        let mut orders = Vec::new();

        for target in targets {
            let key = target.symbol.value.clone();
            let current_qty = securities
                .get(&key)
                .map(|s| s.current_quantity)
                .unwrap_or(Decimal::ZERO);

            let delta = target.quantity - current_qty;
            if delta != Decimal::ZERO {
                orders.push(OrderRequest {
                    symbol: target.symbol.clone(),
                    quantity: delta,
                    order_type: ExecutionOrderType::Market,
                    limit_price: None,
                    tag: "ImmediateExecutionModel".to_string(),
                });
            }
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    fn name(&self) -> &str {
        "ImmediateExecutionModel"
    }
}
