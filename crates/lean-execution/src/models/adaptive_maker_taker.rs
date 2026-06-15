use std::collections::HashMap;

use crate::execution_model::{
    ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest, SecurityData,
};
use crate::models::passive_maker::PassiveMakerExecutionModel;
use lean_core::Symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Cross immediately when the spread is already cheap; otherwise try a
/// post-only maker order before falling back to taker execution.
pub struct AdaptiveMakerTakerExecutionModel {
    pub accepting_spread_percent: Decimal,
    passive: PassiveMakerExecutionModel,
    tight_targets: HashMap<String, (Symbol, Decimal)>,
}

impl AdaptiveMakerTakerExecutionModel {
    pub fn new(
        accepting_spread_percent: Decimal,
        max_passive_attempts: usize,
        adverse_selection_threshold: Decimal,
    ) -> Self {
        Self {
            accepting_spread_percent: accepting_spread_percent.abs(),
            passive: PassiveMakerExecutionModel::new(
                max_passive_attempts,
                adverse_selection_threshold,
            ),
            tight_targets: HashMap::new(),
        }
    }

    fn spread_is_tight(&self, security: &SecurityData) -> bool {
        let (Some(bid), Some(ask)) = (security.bid, security.ask) else {
            return false;
        };
        bid > Decimal::ZERO
            && ask > Decimal::ZERO
            && security.price > Decimal::ZERO
            && (ask - bid) / security.price <= self.accepting_spread_percent
    }
}

impl Default for AdaptiveMakerTakerExecutionModel {
    fn default() -> Self {
        Self::new(dec!(0.001), 1, dec!(0.005))
    }
}

impl IExecutionModel for AdaptiveMakerTakerExecutionModel {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        let mut passive_targets = Vec::new();
        let mut market_orders = Vec::new();

        for target in targets {
            let key = target.symbol.value.clone();
            if securities
                .get(&key)
                .map(|security| self.spread_is_tight(security))
                .unwrap_or(false)
            {
                self.tight_targets
                    .insert(key, (target.symbol.clone(), target.quantity));
            } else {
                self.tight_targets.remove(&key);
                passive_targets.push(target.clone());
            }
        }

        let tight_snapshot: Vec<_> = self
            .tight_targets
            .iter()
            .map(|(key, (symbol, target_quantity))| (key.clone(), symbol.clone(), *target_quantity))
            .collect();

        for (key, symbol, target_quantity) in tight_snapshot {
            let Some(security) = securities.get(&key) else {
                continue;
            };
            if !self.spread_is_tight(security) {
                self.tight_targets.remove(&key);
                passive_targets.push(ExecutionTarget {
                    symbol,
                    quantity: target_quantity,
                });
                continue;
            }

            let delta = target_quantity - security.current_quantity - security.open_order_quantity;
            if delta == Decimal::ZERO {
                self.tight_targets.remove(&key);
                continue;
            }

            market_orders.push(OrderRequest {
                symbol,
                quantity: delta,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                post_only: false,
                cancel_open_orders: false,
                tag: "AdaptiveMakerTakerExecutionModel tight-spread".to_string(),
            });
        }

        let mut passive_orders = self.passive.execute(&passive_targets, securities);
        market_orders.append(&mut passive_orders);
        market_orders
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        self.passive.on_securities_changed(added, removed);
        for symbol in removed {
            self.tight_targets.remove(&symbol.value);
        }
    }

    fn name(&self) -> &str {
        "AdaptiveMakerTakerExecutionModel"
    }
}
