use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use lean_core::Symbol;
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
        // Merge new targets into the persistent target collection.
        for target in targets {
            let key = target.symbol.value.clone();
            self.targets
                .insert(key, (target.symbol.clone(), target.quantity));
        }

        let mut orders = Vec::new();
        let mut fulfilled = Vec::new();

        for (key, (symbol, target_quantity)) in &self.targets {
            let sec = match securities.get(key) {
                Some(s) => s,
                None => continue,
            };

            let unordered_quantity =
                *target_quantity - sec.current_quantity - sec.open_order_quantity;
            if unordered_quantity == Decimal::ZERO {
                fulfilled.push(key.clone());
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
                symbol: symbol.clone(),
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
