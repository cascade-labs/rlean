use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOpenOrder, ExecutionOrderType, ExecutionTarget, IExecutionModel,
    OrderRequest, SecurityData,
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
    targets: HashMap<u64, (Symbol, Decimal)>,
    states: HashMap<u64, PassiveState>,
}

#[derive(Debug, Clone)]
struct PassiveState {
    target_quantity: Decimal,
    direction: Decimal,
    initial_bid: Decimal,
    initial_ask: Decimal,
    active_limit_price: Decimal,
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

    pub fn remove_target(&mut self, symbol: &Symbol) {
        self.targets.remove(&symbol.id.sid);
        self.states.remove(&symbol.id.sid);
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

    fn reset_state_if_needed<'a>(
        states: &'a mut HashMap<u64, PassiveState>,
        key: &u64,
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
                *key,
                PassiveState {
                    target_quantity,
                    direction,
                    initial_bid: bid,
                    initial_ask: ask,
                    active_limit_price: Decimal::ZERO,
                    passive_attempts: 0,
                },
            );
        }

        (states.get_mut(key).expect("state inserted"), reset)
    }

    fn open_order_quantity(context: &ExecutionContext<'_>, sec: &SecurityData) -> Decimal {
        context.projected_open_order_quantity(&sec.symbol, sec)
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
        order_id: None,
        symbol: symbol.clone(),
        quantity: Decimal::ZERO,
        order_type: ExecutionOrderType::Cancel,
        limit_price: None,
        post_only: false,
        cancel_open_orders: true,
        tag: tag.to_string(),
    }
}

fn update_limit_request(
    order: &ExecutionOpenOrder,
    remaining_quantity: Decimal,
    limit_price: Decimal,
    tag: &str,
) -> OrderRequest {
    OrderRequest {
        order_id: Some(order.id),
        symbol: order.symbol.clone(),
        quantity: order.filled_quantity + remaining_quantity,
        order_type: ExecutionOrderType::Update,
        limit_price: Some(limit_price),
        post_only: order.post_only,
        cancel_open_orders: false,
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

fn should_reprice_passive_order(
    direction: Decimal,
    active_limit_price: Decimal,
    passive_price: Decimal,
) -> bool {
    if active_limit_price <= Decimal::ZERO {
        return true;
    }

    if direction > Decimal::ZERO {
        passive_price > active_limit_price
    } else {
        passive_price < active_limit_price
    }
}

fn single_updateable_passive_order<'a>(
    context: &'a ExecutionContext<'_>,
    symbol: &Symbol,
    direction: Decimal,
    open_order_quantity: Decimal,
) -> Option<&'a ExecutionOpenOrder> {
    if open_order_quantity == Decimal::ZERO || sign(open_order_quantity) != direction {
        return None;
    }

    let mut found = None;
    for order in context.open_orders_for_symbol(symbol) {
        if order.remaining_quantity == Decimal::ZERO {
            continue;
        }
        if order.order_type != ExecutionOrderType::Limit
            || !order.post_only
            || sign(order.remaining_quantity) != direction
            || order.remaining_quantity != open_order_quantity
        {
            return None;
        }
        if found.replace(order).is_some() {
            return None;
        }
    }

    found
}

impl IExecutionModel for PassiveMakerExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
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
            let Some(sec) = context.security(&symbol) else {
                continue;
            };
            let open_order_quantity = Self::open_order_quantity(context, sec);

            let target_delta = context.actual_holding_delta(sec, target_quantity);
            if target_delta == Decimal::ZERO {
                fulfilled.push(key);
                self.states.remove(&key);
                if open_order_quantity != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "PassiveMakerExecutionModel cancel fulfilled target",
                    ));
                }
                continue;
            }

            let (Some(bid), Some(ask)) = (sec.bid, sec.ask) else {
                if open_order_quantity != Decimal::ZERO {
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
                if open_order_quantity != Decimal::ZERO {
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
            let maximum_order_value = self.maximum_order_value;
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
            let passive_price = passive_limit_price(direction, bid, ask);

            if use_taker {
                state.passive_attempts = 0;
                let order_qty = Self::cap_order_quantity(maximum_order_value, sec, target_delta);
                if order_qty == Decimal::ZERO {
                    continue;
                }
                orders.push(OrderRequest {
                    order_id: None,
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
                let open_order_direction = sign(open_order_quantity);
                let quote_reprice = should_reprice_passive_order(
                    direction,
                    state.active_limit_price,
                    passive_price,
                );
                let replace_open_order = open_order_quantity != Decimal::ZERO
                    && (target_changed || open_order_direction != direction || quote_reprice);
                if replace_open_order
                    && !target_changed
                    && open_order_direction == direction
                    && quote_reprice
                {
                    if let Some(order) = single_updateable_passive_order(
                        context,
                        &symbol,
                        direction,
                        open_order_quantity,
                    ) {
                        state.active_limit_price = passive_price;
                        orders.push(update_limit_request(
                            order,
                            order.remaining_quantity,
                            passive_price,
                            "PassiveMakerExecutionModel update post-only",
                        ));
                        continue;
                    }
                }

                let unordered_quantity = if replace_open_order {
                    target_delta
                } else {
                    context.unordered_quantity(&symbol, sec, target_quantity)
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

                let order_qty =
                    Self::cap_order_quantity(maximum_order_value, sec, unordered_quantity);
                if order_qty == Decimal::ZERO {
                    continue;
                }
                state.active_limit_price = passive_price;
                orders.push(OrderRequest {
                    order_id: None,
                    symbol: symbol.clone(),
                    quantity: order_qty,
                    order_type: ExecutionOrderType::Limit,
                    limit_price: Some(passive_price),
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
            self.targets.remove(&sym.id.sid);
            self.states.remove(&sym.id.sid);
        }
    }

    fn name(&self) -> &str {
        "PassiveMakerExecutionModel"
    }
}
