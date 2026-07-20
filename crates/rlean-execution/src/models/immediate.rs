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

        // C# LEAN's OrderTargetsByMarginImpact emits no targets during warmup.
        if context.is_warming_up() {
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
            // C# LEAN indexes algorithm.Securities[target.Symbol]. A missing
            // security is an engine invariant violation, not a target to drop.
            let Some(security) = context.authoritative_security(&symbol) else {
                if context.has_authoritative_algorithm() {
                    panic!(
                        "ImmediateExecutionModel target {} is missing from the algorithm SecurityManager",
                        symbol.value
                    );
                }
                continue;
            };
            let target_quantity =
                crate::execution_model::adjust_by_lot_size(security.lot_size, target_quantity);
            let holdings_quantity = context.authoritative_holdings_quantity(&security);
            if (target_quantity - holdings_quantity).abs() < security.lot_size {
                // C# ImmediateExecutionModel calls PortfolioTargetCollection.ClearFulfilled
                // after OrderTargetsByMarginImpact. That cleanup is independent of HasData
                // and IsTradable, so a reset security can retire its already-fulfilled target
                // before deferred physical removal.
                fulfilled.push(key);
                continue;
            }

            if !context.security_has_data(&security) || !context.security_is_tradable(&security) {
                continue;
            }

            let delta = crate::execution_model::adjust_by_lot_size(
                security.lot_size,
                target_quantity - context.authoritative_projected_quantity(&symbol, &security),
            );
            if delta == Decimal::ZERO {
                continue;
            }
            if !context.above_minimum_order_margin_portfolio_percentage(&security, delta) {
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
