use std::collections::HashMap;

use crate::execution_model::{
    ExecutionContext, ExecutionOrderType, ExecutionTarget, IExecutionModel, OrderRequest,
    SecurityData,
};
use crate::models::maker_then_taker::MakerThenTakerExecutionModel;
use lean_core::{Symbol, TimeSpan};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Cross immediately when the spread is already cheap; otherwise try a
/// post-only maker order before falling back to taker execution.
pub struct AdaptiveMakerTakerExecutionModel {
    pub accepting_spread_percent: Decimal,
    passive: MakerThenTakerExecutionModel,
    tight_targets: HashMap<String, (Symbol, Decimal, String)>,
}

impl AdaptiveMakerTakerExecutionModel {
    pub fn new(
        accepting_spread_percent: Decimal,
        max_passive_attempts: usize,
        adverse_selection_threshold: Decimal,
    ) -> Self {
        let passive_minutes = max_passive_attempts.max(1) as i64;
        Self::with_passive_duration(
            accepting_spread_percent,
            TimeSpan::from_mins(passive_minutes),
            adverse_selection_threshold,
        )
    }

    pub fn with_passive_duration(
        accepting_spread_percent: Decimal,
        passive_duration: TimeSpan,
        adverse_selection_threshold: Decimal,
    ) -> Self {
        Self {
            accepting_spread_percent: accepting_spread_percent.abs(),
            passive: MakerThenTakerExecutionModel::new(
                passive_duration,
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

    fn open_order_quantity(context: &ExecutionContext<'_>, security: &SecurityData) -> Decimal {
        context.projected_open_order_quantity(&security.symbol, security)
    }
}

impl Default for AdaptiveMakerTakerExecutionModel {
    fn default() -> Self {
        Self::with_passive_duration(dec!(0.001), TimeSpan::from_mins(5), dec!(0.005))
    }
}

impl IExecutionModel for AdaptiveMakerTakerExecutionModel {
    fn execute_with_context(
        &mut self,
        targets: &[ExecutionTarget],
        context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        let mut passive_targets = Vec::new();
        let mut market_orders = Vec::new();

        for target in targets {
            let key = target.symbol.value.clone();
            if context
                .securities
                .get(&key)
                .map(|security| self.spread_is_tight(security))
                .unwrap_or(false)
            {
                self.passive.remove_target(&target.symbol);
                self.tight_targets.insert(
                    key,
                    (target.symbol.clone(), target.quantity, target.tag.clone()),
                );
            } else {
                self.tight_targets.remove(&key);
                passive_targets.push(target.clone());
            }
        }

        let mut tight_snapshot: Vec<_> = self
            .tight_targets
            .iter()
            .map(|(key, (symbol, target_quantity, _))| {
                (key.clone(), symbol.clone(), *target_quantity)
            })
            .collect();
        context.sort_targets_by_margin_impact(&mut tight_snapshot);

        for (key, symbol, target_quantity) in tight_snapshot {
            let Some(security) = context.securities.get(&key) else {
                continue;
            };
            let open_order_quantity = Self::open_order_quantity(context, security);
            if !self.spread_is_tight(security) {
                let tag = self
                    .tight_targets
                    .get(&key)
                    .map(|(_, _, tag)| tag.clone())
                    .unwrap_or_default();
                self.tight_targets.remove(&key);
                passive_targets.push(ExecutionTarget {
                    symbol,
                    quantity: target_quantity,
                    tag,
                });
                continue;
            }

            let holding_delta = context.actual_holding_delta(security, target_quantity);
            if holding_delta == Decimal::ZERO {
                self.tight_targets.remove(&key);
                if open_order_quantity != Decimal::ZERO {
                    market_orders.push(OrderRequest {
                        order_id: None,
                        symbol,
                        quantity: Decimal::ZERO,
                        order_type: ExecutionOrderType::Cancel,
                        limit_price: None,
                        post_only: false,
                        cancel_open_orders: true,
                        tag: "AdaptiveMakerTakerExecutionModel cancel stale passive".to_string(),
                    });
                }
                continue;
            }

            let order_quantity = if open_order_quantity != Decimal::ZERO {
                holding_delta
            } else {
                context.unordered_quantity(&symbol, security, target_quantity)
            };
            if order_quantity == Decimal::ZERO {
                continue;
            }

            market_orders.push(OrderRequest {
                order_id: None,
                symbol,
                quantity: order_quantity,
                order_type: ExecutionOrderType::Market,
                limit_price: None,
                post_only: false,
                cancel_open_orders: open_order_quantity != Decimal::ZERO,
                tag: "AdaptiveMakerTakerExecutionModel tight-spread".to_string(),
            });
        }

        let mut passive_orders = self.passive.execute_with_context(&passive_targets, context);
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
