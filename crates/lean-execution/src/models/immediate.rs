use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
    SecurityData,
};
use lean_core::{DateTime, Symbol};
use rust_decimal::Decimal;

/// Immediately submits market orders to achieve desired portfolio targets.
///
/// Mirrors C# ImmediateExecutionModel: targets are retained in a collection and
/// ordered by margin impact until projected holdings satisfy them.
pub struct ImmediateExecutionModel {
    targets: HashMap<String, (Symbol, Decimal)>,
}

impl ImmediateExecutionModel {
    pub fn new() -> Self {
        Self {
            targets: HashMap::new(),
        }
    }

    fn execute_internal(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        for target in targets {
            self.targets.insert(
                target.symbol.value.clone(),
                (target.symbol.clone(), target.quantity),
            );
        }

        if self.targets.is_empty() {
            return Vec::new();
        }

        let mut orders = Vec::new();
        let mut fulfilled = Vec::new();
        let mut target_snapshot: Vec<_> = self
            .targets
            .iter()
            .map(|(key, (symbol, quantity))| (key.clone(), symbol.clone(), *quantity))
            .collect();
        context.sort_targets_by_margin_impact(&mut target_snapshot);

        for (key, symbol, target_quantity) in target_snapshot {
            let Some(security) = context.securities.get(&key) else {
                continue;
            };

            if context.actual_holding_delta(security, target_quantity) == Decimal::ZERO {
                fulfilled.push(key);
                continue;
            }

            let delta = context.unordered_quantity(&symbol, security, target_quantity);
            if delta == Decimal::ZERO {
                continue;
            }

            orders.push(OrderRequest {
                order_id: None,
                symbol,
                quantity: delta,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                post_only: false,
                cancel_open_orders: false,
                tag: "ImmediateExecutionModel".to_string(),
            });
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
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
        let open_orders = Vec::new();
        let context = ExecutionContext::new(DateTime::MIN, securities, &open_orders, Decimal::ZERO);
        self.execute_internal(targets, &context)
    }

    fn execute_with_context(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        self.execute_internal(targets, context)
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    fn name(&self) -> &str {
        "ImmediateExecutionModel"
    }
}
