use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
    SecurityData,
};
use lean_core::{DateTime, Symbol, TimeSpan};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Execution model that submits orders while current price is more favorable than VWAP.
///
/// Mirrors C# VolumeWeightedAveragePriceExecutionModel:
/// - Only executes when bid < VWAP (buying) or ask > VWAP (selling)
/// - Limits order size to `participation_rate * current_bar_volume` per call
/// - Keeps portfolio targets across bars and recomputes unordered quantity
pub struct VwapExecutionModel {
    /// Maximum fraction of current bar volume to submit per bar (default 0.01).
    pub participation_rate: Decimal,
    /// Desired target quantity per symbol (ticker -> target quantity).
    targets: HashMap<String, (Symbol, Decimal)>,
    vwap: HashMap<String, VwapState>,
}

#[derive(Debug, Clone)]
struct VwapState {
    day: Option<i64>,
    sum_volume: Decimal,
    sum_price_times_volume: Decimal,
    value: Decimal,
}

impl VwapExecutionModel {
    pub fn new(participation_rate: Decimal) -> Self {
        Self {
            participation_rate,
            targets: HashMap::new(),
            vwap: HashMap::new(),
        }
    }

    fn update_vwap(&mut self, sec: &SecurityData) {
        let Some(volume) = sec.volume else {
            return;
        };

        let vwap_price = sec.vwap_price.unwrap_or(sec.price);
        if vwap_price <= Decimal::ZERO {
            return;
        }

        let key = sec.symbol.value.to_string();
        let state = self.vwap.entry(key).or_insert_with(|| VwapState {
            day: None,
            sum_volume: Decimal::ZERO,
            sum_price_times_volume: Decimal::ZERO,
            value: Decimal::ZERO,
        });

        if let Some(end_time) = sec.end_time {
            let day = end_time.0.div_euclid(TimeSpan::ONE_DAY.nanos);
            if state.day != Some(day) {
                state.day = Some(day);
                state.sum_volume = Decimal::ZERO;
                state.sum_price_times_volume = Decimal::ZERO;
                state.value = Decimal::ZERO;
            }
        }

        if volume <= Decimal::ZERO {
            state.value = sec.price;
            return;
        }

        state.sum_volume += volume;
        state.sum_price_times_volume += vwap_price * volume;
        state.value = state.sum_price_times_volume / state.sum_volume;
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

impl Default for VwapExecutionModel {
    fn default() -> Self {
        Self::new(dec!(0.01))
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

impl IExecutionModel for VwapExecutionModel {
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
        for sec in context.securities.values() {
            self.update_vwap(sec);
        }

        // Merge new targets into the persistent target collection.
        for target in targets {
            let key = target.symbol.value.to_string();
            self.targets
                .insert(key, (target.symbol.clone(), target.quantity));
        }

        let mut orders = Vec::new();
        let mut fulfilled = Vec::new();

        let participation_rate = self.participation_rate;
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

            let vwap = self
                .vwap
                .get(&key)
                .map(|state| state.value)
                .unwrap_or(Decimal::ZERO);
            if vwap <= Decimal::ZERO {
                continue;
            }

            let is_buy = unordered_quantity > Decimal::ZERO;
            let price_favorable = if is_buy {
                let bid = sec.bid.unwrap_or(price);
                bid < vwap
            } else {
                let ask = sec.ask.unwrap_or(price);
                ask > vwap
            };

            if !price_favorable {
                continue;
            }

            let max_order_size = sec.volume.unwrap_or(Decimal::ZERO) * participation_rate;
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
                tag: "VolumeWeightedAveragePriceExecutionModel".to_string(),
            });
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.targets.remove(sym.value.as_ref());
            self.vwap.remove(sym.value.as_ref());
        }
    }

    fn name(&self) -> &str {
        "VolumeWeightedAveragePriceExecutionModel"
    }
}
