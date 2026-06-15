use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use lean_core::Symbol;
use rust_decimal::Decimal;

/// Passive maker execution model for venues that support post-only limit orders.
///
/// The model maintains portfolio targets like LEAN framework execution models,
/// replaces each symbol's stale passive quote before submitting a new one, and
/// crosses the spread only after repeated passive misses or a quote move against
/// the target direction.
pub struct PassiveMakerExecutionModel {
    /// Number of consecutive passive attempts before crossing as a taker.
    pub max_passive_attempts: usize,
    /// Fractional adverse quote move that triggers taker fallback.
    pub adverse_selection_threshold: Decimal,
    /// Optional notional cap per submitted order. Zero disables the cap.
    pub maximum_order_value: Decimal,
    targets: HashMap<String, (Symbol, Decimal)>,
    states: HashMap<String, PassiveState>,
}

#[derive(Debug, Clone)]
struct PassiveState {
    target_quantity: Decimal,
    direction: Decimal,
    initial_bid: Decimal,
    initial_ask: Decimal,
    passive_attempts: usize,
}

impl PassiveMakerExecutionModel {
    pub fn new(max_passive_attempts: usize, adverse_selection_threshold: Decimal) -> Self {
        Self::with_maximum_order_value(
            max_passive_attempts,
            adverse_selection_threshold,
            Decimal::ZERO,
        )
    }

    pub fn with_maximum_order_value(
        max_passive_attempts: usize,
        adverse_selection_threshold: Decimal,
        maximum_order_value: Decimal,
    ) -> Self {
        Self {
            max_passive_attempts,
            adverse_selection_threshold: adverse_selection_threshold.abs(),
            maximum_order_value: maximum_order_value.abs(),
            targets: HashMap::new(),
            states: HashMap::new(),
        }
    }

    fn cap_order_quantity(&self, sec: &SecurityData, quantity: Decimal) -> Decimal {
        if self.maximum_order_value <= Decimal::ZERO || sec.price <= Decimal::ZERO {
            return adjust_by_lot_size(sec.lot_size, quantity);
        }

        let capped = (self.maximum_order_value / sec.price).min(quantity.abs()) * sign(quantity);
        adjust_by_lot_size(sec.lot_size, capped)
    }

    fn reset_state_if_needed<'a>(
        states: &'a mut HashMap<String, PassiveState>,
        key: &str,
        target_quantity: Decimal,
        direction: Decimal,
        bid: Decimal,
        ask: Decimal,
    ) -> (&'a mut PassiveState, bool) {
        let reset = states
            .get(key)
            .map(|state| {
                state.target_quantity != target_quantity
                    || state.direction != direction
                    || state.initial_bid <= Decimal::ZERO
                    || state.initial_ask <= Decimal::ZERO
            })
            .unwrap_or(true);

        if reset {
            states.insert(
                key.to_string(),
                PassiveState {
                    target_quantity,
                    direction,
                    initial_bid: bid,
                    initial_ask: ask,
                    passive_attempts: 0,
                },
            );
        }

        (states.get_mut(key).expect("state inserted"), reset)
    }
}

impl Default for PassiveMakerExecutionModel {
    fn default() -> Self {
        Self::new(3, Decimal::new(10, 4))
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

fn adjust_by_lot_size(lot_size: Decimal, quantity: Decimal) -> Decimal {
    if lot_size <= Decimal::ZERO || quantity == Decimal::ZERO {
        return quantity;
    }

    let abs_quantity = quantity.abs();
    let remainder = abs_quantity % lot_size;
    let adjusted = abs_quantity - remainder;
    adjusted * sign(quantity)
}

fn cancel_request(symbol: &Symbol, tag: &str) -> OrderRequest {
    OrderRequest {
        symbol: symbol.clone(),
        quantity: Decimal::ZERO,
        order_type: ExecutionOrderType::Cancel,
        limit_price: None,
        post_only: false,
        cancel_open_orders: true,
        tag: tag.to_string(),
    }
}

impl IExecutionModel for PassiveMakerExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        for target in targets {
            let key = target.symbol.value.clone();
            self.targets
                .insert(key, (target.symbol.clone(), target.quantity));
        }

        let mut orders = Vec::new();
        let mut fulfilled = Vec::new();

        let target_snapshot: Vec<_> = self
            .targets
            .iter()
            .map(|(key, (symbol, target_quantity))| (key.clone(), symbol.clone(), *target_quantity))
            .collect();

        for (key, symbol, target_quantity) in target_snapshot {
            let Some(sec) = securities.get(&key) else {
                continue;
            };

            let target_delta = target_quantity - sec.current_quantity;
            if target_delta == Decimal::ZERO {
                fulfilled.push(key.clone());
                self.states.remove(&key);
                if sec.open_order_quantity != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "PassiveMakerExecutionModel cancel fulfilled target",
                    ));
                }
                continue;
            }

            let (Some(bid), Some(ask)) = (sec.bid, sec.ask) else {
                if sec.open_order_quantity != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "PassiveMakerExecutionModel cancel missing quote",
                    ));
                }
                continue;
            };
            if bid <= Decimal::ZERO
                || ask <= Decimal::ZERO
                || bid >= ask
                || sec.price <= Decimal::ZERO
            {
                if sec.open_order_quantity != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "PassiveMakerExecutionModel cancel invalid quote",
                    ));
                }
                continue;
            }

            let direction = sign(target_delta);
            let adverse_selection_threshold = self.adverse_selection_threshold;
            let max_passive_attempts = self.max_passive_attempts;
            let (state, target_changed) = Self::reset_state_if_needed(
                &mut self.states,
                &key,
                target_quantity,
                direction,
                bid,
                ask,
            );

            let adverse_move = if direction > Decimal::ZERO {
                ask >= state.initial_ask * (Decimal::ONE + adverse_selection_threshold)
            } else {
                bid <= state.initial_bid * (Decimal::ONE - adverse_selection_threshold)
            };
            let use_taker = state.passive_attempts >= max_passive_attempts || adverse_move;

            if use_taker {
                state.passive_attempts = 0;
                let order_qty = self.cap_order_quantity(sec, target_delta);
                if order_qty == Decimal::ZERO {
                    continue;
                }
                orders.push(OrderRequest {
                    symbol: symbol.clone(),
                    quantity: order_qty,
                    order_type: ExecutionOrderType::Market,
                    limit_price: None,
                    post_only: false,
                    cancel_open_orders: true,
                    tag: if adverse_move {
                        "PassiveMakerExecutionModel taker adverse-selection".to_string()
                    } else {
                        "PassiveMakerExecutionModel taker passive-timeout".to_string()
                    },
                });
            } else {
                state.passive_attempts += 1;
                let open_order_direction = sign(sec.open_order_quantity);
                let replace_open_order = sec.open_order_quantity != Decimal::ZERO
                    && (target_changed || open_order_direction != direction);
                let unordered_quantity = if replace_open_order {
                    target_delta
                } else {
                    target_delta - sec.open_order_quantity
                };

                if unordered_quantity == Decimal::ZERO {
                    continue;
                }
                if sign(unordered_quantity) != direction {
                    orders.push(cancel_request(
                        &symbol,
                        "PassiveMakerExecutionModel cancel overordered target",
                    ));
                    continue;
                }

                let order_qty = self.cap_order_quantity(sec, unordered_quantity);
                if order_qty == Decimal::ZERO {
                    continue;
                }
                orders.push(OrderRequest {
                    symbol: symbol.clone(),
                    quantity: order_qty,
                    order_type: ExecutionOrderType::Limit,
                    limit_price: Some(if direction > Decimal::ZERO { bid } else { ask }),
                    post_only: true,
                    cancel_open_orders: replace_open_order,
                    tag: "PassiveMakerExecutionModel post-only".to_string(),
                });
            }
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for sym in removed {
            self.targets.remove(&sym.value);
            self.states.remove(&sym.value);
        }
    }

    fn name(&self) -> &str {
        "PassiveMakerExecutionModel"
    }
}
