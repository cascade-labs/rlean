use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use lean_core::Symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Execution model that submits orders while current price is more favorable than VWAP.
///
/// Mirrors C# VolumeWeightedAveragePriceExecutionModel:
/// - Only executes when bid < VWAP (buying) or ask > VWAP (selling)
/// - Limits order size to `participation_rate * average_daily_volume` per call
/// - Tracks remaining quantity per symbol across bars
pub struct VwapExecutionModel {
    /// Maximum fraction of average daily volume to submit per bar (default 0.20)
    pub participation_rate: Decimal,
    /// Remaining quantity to fill per symbol (ticker -> remaining delta)
    pending: HashMap<String, Decimal>,
}

impl VwapExecutionModel {
    pub fn new(participation_rate: Decimal) -> Self {
        Self {
            participation_rate,
            pending: HashMap::new(),
        }
    }
}

impl Default for VwapExecutionModel {
    fn default() -> Self {
        // C# uses 1% of current bar volume; we default to 20% of avg daily volume
        Self::new(dec!(0.20))
    }
}

impl IExecutionModel for VwapExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        // Merge new targets into pending map (update desired delta)
        for target in targets {
            let key = target.symbol.value.clone();
            let current_qty = securities
                .get(&key)
                .map(|s| s.current_quantity)
                .unwrap_or(Decimal::ZERO);
            let delta = target.quantity - current_qty;
            // Overwrite with the latest desired delta (new targets override pending)
            self.pending.insert(key, delta);
        }

        let mut orders = Vec::new();

        for (key, remaining) in &mut self.pending {
            if *remaining == Decimal::ZERO {
                continue;
            }

            let sec = match securities.get(key) {
                Some(s) => s,
                None => continue,
            };

            // Determine if price is favorable (proxy VWAP with current price)
            // For buying: favorable if bid < price (bid below "VWAP")
            // For selling: favorable if ask > price (ask above "VWAP")
            // We use mid-price as the VWAP proxy since we don't have a real VWAP indicator here.
            let price = sec.price;
            if price == Decimal::ZERO {
                continue;
            }

            let is_buy = *remaining > Decimal::ZERO;
            let price_favorable = if is_buy {
                // buying: bid < vwap proxy (price); if bid is below mid, favorable
                sec.bid.map(|b| b < price).unwrap_or(true)
            } else {
                // selling: ask > vwap proxy (price); if ask is above mid, favorable
                sec.ask.map(|a| a > price).unwrap_or(true)
            };

            if !price_favorable {
                continue;
            }

            // Copy remaining to a plain Decimal to avoid &mut Decimal type issues
            let rem = *remaining;

            // Calculate max slice size based on participation rate and average volume
            let max_slice = sec
                .average_volume
                .filter(|&v| v > Decimal::ZERO)
                .map(|v| v * self.participation_rate)
                .unwrap_or_else(|| rem.abs()); // if no volume data, submit all

            let order_qty = if is_buy {
                rem.min(max_slice)
            } else {
                // remaining is negative for sells
                let neg_max = -max_slice;
                rem.max(neg_max)
            };

            if order_qty == Decimal::ZERO {
                continue;
            }

            orders.push(OrderRequest {
                symbol: sec.symbol.clone(),
                quantity: order_qty,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                tag: "VwapExecutionModel".to_string(),
            });

            *remaining -= order_qty;
        }

        // Remove fulfilled entries
        self.pending.retain(|_, v| *v != Decimal::ZERO);

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.pending.remove(&sym.value);
        }
    }

    fn name(&self) -> &str {
        "VwapExecutionModel"
    }
}
