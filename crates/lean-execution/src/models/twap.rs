use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use lean_core::Symbol;
use rust_decimal::Decimal;

/// State tracked per-symbol for TWAP slicing.
#[derive(Debug, Clone)]
struct TwapState {
    /// Total quantity to execute (signed)
    total: Decimal,
    /// Quantity submitted so far (signed)
    submitted: Decimal,
    /// How many slices total
    num_slices: u32,
    /// How many slices have been submitted
    slices_submitted: u32,
}

/// Time-Weighted Average Price execution model.
///
/// Divides the total order into N equal slices submitted across N consecutive bars.
/// Each call to `execute()` submits at most one slice per symbol.
pub struct TwapExecutionModel {
    /// Number of equal time slices (default: 4)
    pub num_slices: u32,
    state: HashMap<String, TwapState>,
}

impl TwapExecutionModel {
    pub fn new(num_slices: u32) -> Self {
        assert!(num_slices > 0, "num_slices must be > 0");
        Self {
            num_slices,
            state: HashMap::new(),
        }
    }
}

impl Default for TwapExecutionModel {
    fn default() -> Self {
        Self::new(4)
    }
}

impl IExecutionModel for TwapExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        // Register or update targets
        for target in targets {
            let key = target.symbol.value.clone();
            let current_qty = securities
                .get(&key)
                .map(|s| s.current_quantity)
                .unwrap_or(Decimal::ZERO);
            let total_delta = target.quantity - current_qty;

            self.state.insert(
                key,
                TwapState {
                    total: total_delta,
                    submitted: Decimal::ZERO,
                    num_slices: self.num_slices,
                    slices_submitted: 0,
                },
            );
        }

        let mut orders = Vec::new();

        for (key, state) in &mut self.state {
            if state.slices_submitted >= state.num_slices {
                continue;
            }
            if state.total == Decimal::ZERO {
                continue;
            }

            let sec = match securities.get(key) {
                Some(s) => s,
                None => continue,
            };

            // Calculate remaining slices and quantity
            let remaining = state.total - state.submitted;
            let slices_left = state.num_slices - state.slices_submitted;

            // Each slice is 1/num_slices of total; round to reasonable precision
            // Use ceiling for positive, floor for negative to avoid under-execution
            let slice_qty = remaining
                / Decimal::from(slices_left);

            // Truncate to avoid tiny fractional shares
            // (round toward zero)
            let slice_qty = if slice_qty > Decimal::ZERO {
                slice_qty.trunc()
            } else {
                -(-slice_qty).trunc()
            };

            if slice_qty == Decimal::ZERO {
                // Mark as done if nothing meaningful left
                state.slices_submitted = state.num_slices;
                continue;
            }

            orders.push(OrderRequest {
                symbol: sec.symbol.clone(),
                quantity: slice_qty,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                tag: format!(
                    "TwapExecutionModel slice {}/{}",
                    state.slices_submitted + 1,
                    state.num_slices
                ),
            });

            state.submitted += slice_qty;
            state.slices_submitted += 1;
        }

        // Remove completed states
        self.state
            .retain(|_, s| s.slices_submitted < s.num_slices && s.total != Decimal::ZERO);

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.state.remove(&sym.value);
        }
    }

    fn name(&self) -> &str {
        "TwapExecutionModel"
    }
}
