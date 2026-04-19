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
    /// Pending targets: symbol ticker -> desired delta
    pending: HashMap<String, (Symbol, Decimal)>,
}

impl SpreadExecutionModel {
    pub fn new(accepting_spread_percent: Decimal) -> Self {
        Self {
            accepting_spread_percent: accepting_spread_percent.abs(),
            pending: HashMap::new(),
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
        // Merge new targets into pending, computing delta vs current holdings
        for target in targets {
            let key = target.symbol.value.clone();
            let current_qty = securities
                .get(&key)
                .map(|s| s.current_quantity)
                .unwrap_or(Decimal::ZERO);
            let delta = target.quantity - current_qty;
            self.pending.insert(key, (target.symbol.clone(), delta));
        }

        let mut orders = Vec::new();

        for (key, (symbol, remaining)) in &mut self.pending {
            if *remaining == Decimal::ZERO {
                continue;
            }

            let sec = match securities.get(key) {
                Some(s) => s,
                None => continue,
            };

            // Check spread acceptability: (ask - bid) / price <= threshold
            // Requires both bid and ask to be available and price > 0
            let price = sec.price;
            if price <= Decimal::ZERO {
                continue;
            }

            let spread_ok = match (sec.bid, sec.ask) {
                (Some(bid), Some(ask)) if bid > Decimal::ZERO && ask > Decimal::ZERO => {
                    (ask - bid) / price <= self.accepting_spread_percent
                }
                // If bid/ask not available, fall back to allowing execution
                _ => true,
            };

            if !spread_ok {
                continue;
            }

            orders.push(OrderRequest {
                symbol: symbol.clone(),
                quantity: *remaining,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                tag: "SpreadExecutionModel".to_string(),
            });

            *remaining = Decimal::ZERO;
        }

        // Remove fulfilled entries
        self.pending.retain(|_, (_, r)| *r != Decimal::ZERO);

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.pending.remove(&sym.value);
        }
    }

    fn name(&self) -> &str {
        "SpreadExecutionModel"
    }
}
