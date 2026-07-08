use std::collections::{HashMap, VecDeque};

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
    SecurityData,
};
use lean_core::Symbol;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Execution model that submits market orders when price has moved at least `deviations` standard
/// deviations from the mean in the favorable direction.
///
/// Mirrors C# StandardDeviationExecutionModel:
/// - For buys: execute if bid < SMA - (deviations * std_dev)  → price dipped below mean
/// - For sells: execute if ask > SMA + (deviations * std_dev) → price spiked above mean
pub struct StandardDeviationExecutionModel {
    /// Period of the rolling SMA/std-dev indicators.
    pub period: usize,
    /// Number of std deviations required before executing (default: 2.0)
    pub deviations: Decimal,
    /// Maximum order notional in account currency per execution slice.
    ///
    /// Mirrors C# StandardDeviationExecutionModel.MaximumOrderValue.
    pub maximum_order_value: Decimal,
    /// Desired target quantity per symbol (ticker -> target quantity).
    targets: HashMap<u64, (Symbol, Decimal)>,
    prices: HashMap<u64, VecDeque<Decimal>>,
}

impl StandardDeviationExecutionModel {
    pub fn new(period: usize, deviations: Decimal) -> Self {
        Self::with_maximum_order_value(period, deviations, dec!(20000))
    }

    pub fn with_maximum_order_value(
        period: usize,
        deviations: Decimal,
        maximum_order_value: Decimal,
    ) -> Self {
        Self {
            period: period.max(1),
            deviations,
            maximum_order_value,
            targets: HashMap::new(),
            prices: HashMap::new(),
        }
    }

    fn update_price(&mut self, security: &SecurityData) {
        if security.price <= Decimal::ZERO {
            return;
        }

        let window = self.prices.entry(security.symbol.id.sid).or_default();
        window.push_back(security.price);
        while window.len() > self.period {
            window.pop_front();
        }
    }

    fn ready_mean_std_dev(&self, key: &u64) -> Option<(Decimal, Decimal)> {
        let window = self.prices.get(key)?;
        if window.len() < self.period {
            return None;
        }

        let count = Decimal::from(window.len());
        let mean = window.iter().copied().sum::<Decimal>() / count;
        let variance = window
            .iter()
            .map(|price| {
                let diff = *price - mean;
                diff * diff
            })
            .sum::<Decimal>()
            / count;
        let std_dev =
            Decimal::from_f64(variance.to_f64().unwrap_or(0.0).sqrt()).unwrap_or(Decimal::ZERO);
        Some((mean, std_dev))
    }

    fn adjust_by_lot_size(lot_size: Decimal, quantity: Decimal) -> Decimal {
        if lot_size <= Decimal::ZERO || quantity == Decimal::ZERO {
            return quantity;
        }

        let abs_quantity = quantity.abs();
        let mut remainder = abs_quantity % lot_size;
        let missing_for_lot_size = lot_size - remainder;
        if missing_for_lot_size < lot_size / dec!(1000000) {
            remainder -= lot_size;
        }

        (abs_quantity - remainder) * sign(quantity)
    }
}

fn sign(value: Decimal) -> Decimal {
    if value < Decimal::ZERO {
        -Decimal::ONE
    } else if value > Decimal::ZERO {
        Decimal::ONE
    } else {
        Decimal::ZERO
    }
}

impl Default for StandardDeviationExecutionModel {
    fn default() -> Self {
        Self::new(60, dec!(2.0))
    }
}

impl IExecutionModel for StandardDeviationExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        for sec in context.securities.values() {
            self.update_price(sec);
        }

        // Merge new targets into the persistent target collection.
        for target in targets {
            let key = target.symbol.id.sid;
            self.targets
                .insert(key, (target.symbol.clone(), target.quantity));
        }

        let mut orders = Vec::new();
        let mut fulfilled = Vec::new();

        let mut target_snapshot: Vec<_> = self
            .targets
            .iter()
            .map(|(key, (symbol, target_quantity))| (*key, symbol.clone(), *target_quantity))
            .collect();
        context.sort_targets_by_margin_impact(&mut target_snapshot);

        for (key, symbol, target_quantity) in target_snapshot {
            let sec = match context.security(&symbol) {
                Some(s) => s,
                None => continue,
            };

            if context.actual_holding_delta(sec, target_quantity) == Decimal::ZERO {
                fulfilled.push(key);
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

            let Some((mean, std_dev)) = self.ready_mean_std_dev(&key) else {
                continue;
            };
            if std_dev <= Decimal::ZERO {
                continue;
            }

            let threshold = self.deviations * std_dev;
            let is_buy = unordered_quantity > Decimal::ZERO;
            let price_favorable = if is_buy {
                let bid = sec.bid.unwrap_or(price);
                bid < mean - threshold
            } else {
                let ask = sec.ask.unwrap_or(price);
                ask > mean + threshold
            };

            if !price_favorable {
                continue;
            }

            let max_order_size = if self.maximum_order_value > Decimal::ZERO {
                self.maximum_order_value / price
            } else {
                Decimal::ZERO
            };
            let order_size = max_order_size.min(unordered_quantity.abs());
            let order_qty =
                Self::adjust_by_lot_size(sec.lot_size, order_size) * sign(unordered_quantity);

            if order_qty == Decimal::ZERO {
                continue;
            }

            orders.push(OrderRequest {
                order_id: None,
                symbol,
                quantity: order_qty,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                post_only: false,
                cancel_open_orders: false,
                tag: format!(
                    "StandardDeviationExecutionModel period={} deviations={}",
                    self.period, self.deviations
                ),
            });
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.targets.remove(&sym.id.sid);
            self.prices.remove(&sym.id.sid);
        }
    }

    fn name(&self) -> &str {
        "StandardDeviationExecutionModel"
    }
}
