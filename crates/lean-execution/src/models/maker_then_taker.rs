use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
    SecurityData,
};
use lean_core::{DateTime, Symbol, TimeSpan};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Maker-first execution model with a time-based taker deadline.
///
/// The model maintains LEAN-style portfolio targets across calls. For each
/// target it posts a post-only limit at the same-side quote, keeps that order
/// live while the passive window is open, reprices only when the same-side quote
/// improves, and crosses the remaining target delta when the passive deadline or
/// adverse-selection threshold is reached.
pub struct MakerThenTakerExecutionModel {
    pub passive_duration: TimeSpan,
    pub adverse_selection_threshold: Decimal,
    pub maximum_order_value: Decimal,
    targets: HashMap<String, (Symbol, Decimal)>,
    states: HashMap<String, MakerState>,
}

#[derive(Debug, Clone)]
struct MakerState {
    target_quantity: Decimal,
    direction: Decimal,
    start_time: DateTime,
    initial_bid: Decimal,
    initial_ask: Decimal,
    active_limit_price: Decimal,
}

impl MakerThenTakerExecutionModel {
    pub fn new(passive_duration: TimeSpan, adverse_selection_threshold: Decimal) -> Self {
        Self::with_maximum_order_value(passive_duration, adverse_selection_threshold, Decimal::ZERO)
    }

    pub fn with_maximum_order_value(
        passive_duration: TimeSpan,
        adverse_selection_threshold: Decimal,
        maximum_order_value: Decimal,
    ) -> Self {
        Self {
            passive_duration,
            adverse_selection_threshold: adverse_selection_threshold.abs(),
            maximum_order_value: maximum_order_value.abs(),
            targets: HashMap::new(),
            states: HashMap::new(),
        }
    }

    fn cap_order_quantity(
        maximum_order_value: Decimal,
        sec: &SecurityData,
        quantity: Decimal,
    ) -> Decimal {
        if maximum_order_value <= Decimal::ZERO || sec.price <= Decimal::ZERO {
            return adjust_by_lot_size(sec.lot_size, quantity);
        }

        let capped = (maximum_order_value / sec.price).min(quantity.abs()) * sign(quantity);
        adjust_by_lot_size(sec.lot_size, capped)
    }

    fn reset_state_if_needed(
        &mut self,
        key: &str,
        target_quantity: Decimal,
        direction: Decimal,
        now: DateTime,
        bid: Decimal,
        ask: Decimal,
    ) -> (&mut MakerState, bool) {
        let reset = self
            .states
            .get(key)
            .map(|state| state.target_quantity != target_quantity || state.direction != direction)
            .unwrap_or(true);

        if reset {
            self.states.insert(
                key.to_string(),
                MakerState {
                    target_quantity,
                    direction,
                    start_time: now,
                    initial_bid: bid,
                    initial_ask: ask,
                    active_limit_price: Decimal::ZERO,
                },
            );
        }

        (self.states.get_mut(key).expect("state inserted"), reset)
    }
}

impl Default for MakerThenTakerExecutionModel {
    fn default() -> Self {
        Self::new(TimeSpan::from_mins(5), dec!(0.005))
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

fn passive_limit_price(direction: Decimal, bid: Decimal, ask: Decimal) -> Decimal {
    if direction > Decimal::ZERO {
        bid
    } else {
        ask
    }
}

fn should_reprice(direction: Decimal, active_limit_price: Decimal, passive_price: Decimal) -> bool {
    if active_limit_price <= Decimal::ZERO {
        return true;
    }

    if direction > Decimal::ZERO {
        passive_price > active_limit_price
    } else {
        passive_price < active_limit_price
    }
}

impl IExecutionModel for MakerThenTakerExecutionModel {
    fn execute_with_context(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
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
            let Some(sec) = context.securities.get(&key) else {
                continue;
            };

            let target_delta = target_quantity - sec.current_quantity;
            if target_delta == Decimal::ZERO {
                fulfilled.push(key.clone());
                self.states.remove(&key);
                if context.open_order_quantity(&symbol) != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "MakerThenTakerExecutionModel cancel fulfilled target",
                    ));
                }
                continue;
            }

            let (Some(bid), Some(ask)) = (sec.bid, sec.ask) else {
                if context.open_order_quantity(&symbol) != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "MakerThenTakerExecutionModel cancel missing quote",
                    ));
                }
                continue;
            };

            if bid <= Decimal::ZERO
                || ask <= Decimal::ZERO
                || bid > ask
                || sec.price <= Decimal::ZERO
            {
                if context.open_order_quantity(&symbol) != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "MakerThenTakerExecutionModel cancel invalid quote",
                    ));
                }
                continue;
            }

            let direction = sign(target_delta);
            let adverse_selection_threshold = self.adverse_selection_threshold;
            let passive_duration = self.passive_duration;
            let maximum_order_value = self.maximum_order_value;
            let (state, target_changed) = self.reset_state_if_needed(
                &key,
                target_quantity,
                direction,
                context.time,
                bid,
                ask,
            );
            let passive_price = passive_limit_price(direction, bid, ask);
            let adverse_move = if direction > Decimal::ZERO {
                ask >= state.initial_ask * (Decimal::ONE + adverse_selection_threshold)
            } else {
                bid <= state.initial_bid * (Decimal::ONE - adverse_selection_threshold)
            };
            let deadline_reached = passive_duration <= TimeSpan::ZERO
                || (context.time >= state.start_time
                    && context.time - state.start_time >= passive_duration);

            if deadline_reached || adverse_move {
                let order_qty = Self::cap_order_quantity(maximum_order_value, sec, target_delta);
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
                        "MakerThenTakerExecutionModel taker adverse-selection".to_string()
                    } else {
                        "MakerThenTakerExecutionModel taker deadline".to_string()
                    },
                });
                state.start_time = context.time;
                state.active_limit_price = Decimal::ZERO;
                continue;
            }

            let open_order_quantity = context.open_order_quantity(&symbol);
            let open_order_direction = sign(open_order_quantity);
            let replace_open_order = open_order_quantity != Decimal::ZERO
                && (target_changed
                    || open_order_direction != direction
                    || should_reprice(direction, state.active_limit_price, passive_price));
            let unordered_quantity = if replace_open_order {
                target_delta
            } else {
                target_delta - open_order_quantity
            };

            if unordered_quantity == Decimal::ZERO {
                continue;
            }
            if sign(unordered_quantity) != direction {
                orders.push(cancel_request(
                    &symbol,
                    "MakerThenTakerExecutionModel cancel overordered target",
                ));
                continue;
            }

            let order_qty = Self::cap_order_quantity(maximum_order_value, sec, unordered_quantity);
            if order_qty == Decimal::ZERO {
                continue;
            }
            state.active_limit_price = passive_price;
            orders.push(OrderRequest {
                symbol: symbol.clone(),
                quantity: order_qty,
                order_type: ExecutionOrderType::Limit,
                limit_price: Some(passive_price),
                post_only: true,
                cancel_open_orders: replace_open_order,
                tag: "MakerThenTakerExecutionModel post-only".to_string(),
            });
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for symbol in removed {
            self.targets.remove(&symbol.value);
            self.states.remove(&symbol.value);
        }
    }

    fn name(&self) -> &str {
        "MakerThenTakerExecutionModel"
    }
}
