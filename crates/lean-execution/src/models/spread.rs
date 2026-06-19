use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
    SecurityData,
};
use lean_core::{DateTime, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Execution model that only submits orders when the bid-ask spread is acceptably tight.
///
/// Mirrors C# SpreadExecutionModel:
/// - Checks (ask - bid) / price <= accepting_spread_percent
/// - If spread is too wide, defers the order (no order emitted this bar)
/// - If spread is acceptable, fires a market order for the full delta
pub struct SpreadExecutionModel {
    /// Maximum acceptable spread as a fraction of price (default 0.005 = 0.5%)
    pub accepting_spread_percent: Decimal,
    /// Desired target quantity per symbol (ticker -> target quantity).
    targets: HashMap<String, (Symbol, Decimal)>,
}

impl SpreadExecutionModel {
    pub fn new(accepting_spread_percent: Decimal) -> Self {
        Self {
            accepting_spread_percent: accepting_spread_percent.abs(),
            targets: HashMap::new(),
        }
    }
}

impl Default for SpreadExecutionModel {
    fn default() -> Self {
        Self::new(dec!(0.005))
    }
}

impl IExecutionModel for SpreadExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        let open_orders = Vec::new();
        let context = ExecutionContext::new(DateTime::MIN, securities, &open_orders, Decimal::ZERO);
        self.execute_with_context(targets, &context)
    }

    fn execute_with_context(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        // Merge new targets into the persistent target collection.
        for target in targets {
            let key = target.symbol.value.clone();
            self.targets
                .insert(key, (target.symbol.clone(), target.quantity));
        }

        let mut orders = Vec::new();
        let mut fulfilled = Vec::new();

        let mut target_snapshot: Vec<_> = self
            .targets
            .iter()
            .map(|(key, (symbol, target_quantity))| (key.clone(), symbol.clone(), *target_quantity))
            .collect();
        context.sort_targets_by_margin_impact(&mut target_snapshot);

        for (key, symbol, target_quantity) in target_snapshot {
            let sec = match context.securities.get(&key) {
                Some(s) => s,
                None => continue,
            };

            if context.actual_holding_delta(sec, target_quantity) == Decimal::ZERO {
                fulfilled.push(key.clone());
                continue;
            }

            let unordered_quantity = context.unordered_quantity(&symbol, sec, target_quantity);
            if unordered_quantity == Decimal::ZERO {
                continue;
            }

            let price = sec.price;
            if price <= Decimal::ZERO {
                continue;
            }

            let spread_ok = match (sec.bid, sec.ask) {
                (Some(bid), Some(ask)) if bid > Decimal::ZERO && ask > Decimal::ZERO => {
                    (ask - bid) / price <= self.accepting_spread_percent
                }
                _ => false,
            };

            if !spread_ok {
                continue;
            }

            orders.push(OrderRequest {
                order_id: None,
                symbol,
                quantity: unordered_quantity,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                post_only: false,
                cancel_open_orders: false,
                tag: "SpreadExecutionModel".to_string(),
            });
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.targets.remove(&sym.value);
        }
    }

    fn name(&self) -> &str {
        "SpreadExecutionModel"
    }
}
