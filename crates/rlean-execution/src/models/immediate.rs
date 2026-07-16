use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
};
use rlean_core::Symbol;
use rust_decimal::Decimal;

/// Immediately submits market orders to achieve desired portfolio targets.
///
/// Mirrors C# ImmediateExecutionModel: targets are retained in a collection and
/// ordered by margin impact until projected holdings satisfy them.
pub struct ImmediateExecutionModel {
    targets: HashMap<u64, (Symbol, Decimal, String)>,
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
                target.symbol.id.sid,
                (target.symbol.clone(), target.quantity, target.tag.clone()),
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
            .map(|(key, (symbol, quantity, _))| (*key, symbol.clone(), *quantity))
            .collect();
        context.sort_targets_by_margin_impact(&mut target_snapshot);

        for (key, symbol, target_quantity) in target_snapshot {
            let Some(security) = context.security(&symbol) else {
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
            if !context.above_minimum_order_margin_portfolio_percentage(security, delta) {
                continue;
            }

            let tag = self
                .targets
                .get(&key)
                .map(|(_, _, tag)| tag.clone())
                .unwrap_or_default();
            orders.push(OrderRequest {
                order_id: None,
                symbol,
                quantity: delta,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                post_only: false,
                cancel_open_orders: false,
                tag: if tag.is_empty() {
                    "ImmediateExecutionModel".to_string()
                } else {
                    tag
                },
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
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        self.execute_internal(targets, context)
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    fn name(&self) -> &str {
        "ImmediateExecutionModel"
    }
}
