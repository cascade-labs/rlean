use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use super::context::MarginCallContext;
use super::model::{MarginCallModel, MarginCallOrderRequest};
use super::position_group::generate_margin_call_quantity_for_group;

const DEFAULT_MARGIN_BUFFER: Decimal = dec!(0.10);
const WARNING_MARGIN_FRACTION: Decimal = dec!(0.05);

/// Default LEAN margin call model — liquidates losers first when margin is exhausted.
#[derive(Debug, Clone)]
pub struct DefaultMarginCallModel {
    margin_buffer: Decimal,
}

impl DefaultMarginCallModel {
    pub fn new() -> Self {
        Self {
            margin_buffer: DEFAULT_MARGIN_BUFFER,
        }
    }

    pub fn with_margin_buffer(margin_buffer: Decimal) -> Self {
        Self { margin_buffer }
    }
}

impl Default for DefaultMarginCallModel {
    fn default() -> Self {
        Self::new()
    }
}

impl MarginCallModel for DefaultMarginCallModel {
    fn get_margin_call_orders(
        &self,
        ctx: &MarginCallContext,
    ) -> (Vec<MarginCallOrderRequest>, bool) {
        let mut issue_margin_call_warning = false;

        if ctx.total_margin_used <= Decimal::ZERO {
            return (Vec::new(), false);
        }

        let margin_remaining = ctx.margin_remaining();

        if margin_remaining <= ctx.total_portfolio_value * WARNING_MARGIN_FRACTION {
            issue_margin_call_warning = true;
        }

        let mut margin_call_orders = Vec::new();

        if margin_remaining <= Decimal::ZERO
            && ctx.total_margin_used
                > ctx.total_portfolio_value * (Decimal::ONE + self.margin_buffer)
        {
            for group in &ctx.groups.groups {
                let quantity = generate_margin_call_quantity_for_group(
                    group,
                    ctx.total_portfolio_value,
                    ctx.total_margin_used,
                    margin_remaining,
                );
                if !quantity.is_zero() {
                    margin_call_orders
                        .push(MarginCallOrderRequest::new(group.symbol.clone(), quantity));
                }
            }
            issue_margin_call_warning = !margin_call_orders.is_empty();
        }

        (margin_call_orders, issue_margin_call_warning)
    }
}
