use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use lean_core::Symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Execution model that submits market orders when price has moved at least `deviations` standard
/// deviations from the mean in the favorable direction.
///
/// Mirrors C# StandardDeviationExecutionModel:
/// - For buys: execute if bid < SMA - (deviations * std_dev)  → price dipped below mean
/// - For sells: execute if ask > SMA + (deviations * std_dev) → price spiked above mean
///
/// Since we lack a running SMA/STD indicator here, we use the current price as the mean proxy
/// and `security_data.daily_std_dev` as the standard deviation. When daily_std_dev is None,
/// execution is always allowed (no filter applied).
pub struct StandardDeviationExecutionModel {
    /// Number of std deviations required before executing (default: 2.0)
    pub deviations: Decimal,
    /// Pending targets: symbol ticker -> (symbol, remaining delta)
    pending: HashMap<String, (Symbol, Decimal)>,
}

impl StandardDeviationExecutionModel {
    pub fn new(deviations: Decimal) -> Self {
        Self {
            deviations,
            pending: HashMap::new(),
        }
    }
}

impl Default for StandardDeviationExecutionModel {
    fn default() -> Self {
        Self::new(dec!(2.0))
    }
}

impl IExecutionModel for StandardDeviationExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        // Merge new targets into pending
        for target in targets {
            let key = target.symbol.value.clone();
            let current_qty = securities
                .get(&key)
                .map(|s| s.current_quantity)
                .unwrap_or(Decimal::ZERO);
            let delta = target.quantity - current_qty;
            self.pending
                .insert(key, (target.symbol.clone(), delta));
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

            let price = sec.price;
            if price <= Decimal::ZERO {
                continue;
            }

            // Check if price is favorable relative to std dev bands.
            // We use current price as the SMA proxy (no running indicator).
            // Favorable = price has moved enough std devs in the right direction.
            let price_favorable = match sec.daily_std_dev {
                Some(std_dev) if std_dev > Decimal::ZERO => {
                    let threshold = self.deviations * std_dev;
                    let is_buy = *remaining > Decimal::ZERO;
                    if is_buy {
                        // Buy when bid is below price - threshold
                        let bid = sec.bid.unwrap_or(price);
                        bid < price - threshold
                    } else {
                        // Sell when ask is above price + threshold
                        let ask = sec.ask.unwrap_or(price);
                        ask > price + threshold
                    }
                }
                // No std dev data: allow execution unconditionally
                _ => true,
            };

            if !price_favorable {
                continue;
            }

            orders.push(OrderRequest {
                symbol: symbol.clone(),
                quantity: *remaining,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                tag: format!(
                    "StandardDeviationExecutionModel deviations={}",
                    self.deviations
                ),
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
        "StandardDeviationExecutionModel"
    }
}
