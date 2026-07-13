use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOpenOrder, ExecutionOrderType, ExecutionTarget, IExecutionModel,
    OrderRequest, SecurityData,
};
use rlean_core::Symbol;
use rust_decimal::Decimal;

/// Maker execution model that pegs post-only limits one tick inside the spread.
///
/// Buys are priced at `ask - tick`, sells at `bid + tick`, clamped so the order
/// never crosses the current quote. The model keeps the same LEAN framework
/// target-retention semantics as the other execution models and never crosses
/// the remaining quantity as a market order.
pub struct AggressivePostOnlyExecutionModel {
    pub maximum_order_value: Decimal,
    targets: HashMap<u64, (Symbol, Decimal)>,
    states: HashMap<u64, MakerState>,
}

#[derive(Debug, Clone)]
struct MakerState {
    target_quantity: Decimal,
    direction: Decimal,
    active_limit_price: Decimal,
}

impl AggressivePostOnlyExecutionModel {
    pub fn new() -> Self {
        Self::with_maximum_order_value(Decimal::ZERO)
    }

    pub fn with_maximum_order_value(maximum_order_value: Decimal) -> Self {
        Self {
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

    fn reset_state_if_needed(
        &mut self,
        key: &u64,
        target_quantity: Decimal,
        direction: Decimal,
    ) -> (&mut MakerState, bool) {
        let reset = self
            .states
            .get(key)
            .map(|state| state.target_quantity != target_quantity || state.direction != direction)
            .unwrap_or(true);

        if reset {
            self.states.insert(
                *key,
                MakerState {
                    target_quantity,
                    direction,
                    active_limit_price: Decimal::ZERO,
                },
            );
        }

        (self.states.get_mut(key).expect("state inserted"), reset)
    }
}

impl Default for AggressivePostOnlyExecutionModel {
    fn default() -> Self {
        Self::new()
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

fn post_only_inside_price(
    direction: Decimal,
    bid: Decimal,
    ask: Decimal,
    tick: Decimal,
) -> Decimal {
    if direction > Decimal::ZERO {
        let aggressive = ask - tick;
        if aggressive > bid && aggressive < ask {
            aggressive
        } else {
            bid
        }
    } else {
        let aggressive = bid + tick;
        if aggressive < ask && aggressive > bid {
            aggressive
        } else {
            ask
        }
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

impl IExecutionModel for AggressivePostOnlyExecutionModel {
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

            let target_delta = context.actual_holding_delta(sec, target_quantity);
            if target_delta == Decimal::ZERO {
                fulfilled.push(key);
                self.states.remove(&key);
                if context.projected_open_order_quantity(&symbol, sec) != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "AggressivePostOnlyExecutionModel cancel fulfilled target",
                    ));
                }
                continue;
            }

            let (Some(bid), Some(ask)) = (sec.bid, sec.ask) else {
                if context.projected_open_order_quantity(&symbol, sec) != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "AggressivePostOnlyExecutionModel cancel missing quote",
                    ));
                }
                continue;
            };

            if bid <= Decimal::ZERO
                || ask <= Decimal::ZERO
                || bid >= ask
                || sec.price <= Decimal::ZERO
                || sec.minimum_price_variation <= Decimal::ZERO
            {
                if context.projected_open_order_quantity(&symbol, sec) != Decimal::ZERO {
                    orders.push(cancel_request(
                        &symbol,
                        "AggressivePostOnlyExecutionModel cancel invalid quote",
                    ));
                }
                continue;
            }

            let direction = sign(target_delta);
            let maximum_order_value = self.maximum_order_value;
            let (state, target_changed) =
                self.reset_state_if_needed(&key, target_quantity, direction);
            let passive_price =
                post_only_inside_price(direction, bid, ask, sec.minimum_price_variation);

            let open_order_quantity = context.projected_open_order_quantity(&symbol, sec);
            let open_order_direction = sign(open_order_quantity);
            let quote_reprice = should_reprice(direction, state.active_limit_price, passive_price);
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
                        "AggressivePostOnlyExecutionModel update post-only",
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
                    "AggressivePostOnlyExecutionModel cancel overordered target",
                ));
                continue;
            }

            let order_qty = Self::cap_order_quantity(maximum_order_value, sec, unordered_quantity);
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
                tag: "AggressivePostOnlyExecutionModel post-only".to_string(),
            });
        }

        for key in fulfilled {
            self.targets.remove(&key);
        }

        orders
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], removed: &[Symbol]) {
        for symbol in removed {
            self.targets.remove(&symbol.id.sid);
            self.states.remove(&symbol.id.sid);
        }
    }

    fn name(&self) -> &str {
        "AggressivePostOnlyExecutionModel"
    }
}
