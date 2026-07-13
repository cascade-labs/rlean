use rlean_orders::OrderTicket;
use rust_decimal::Decimal;

use super::model::MarginCallExecutionContext;
use super::model::MarginCallOrderRequest;
use super::MarginCallFillProcessor;

/// Execute margin-call orders losers-first until margin is restored.
pub fn execute_margin_call_orders<P: MarginCallFillProcessor>(
    ctx: &mut MarginCallExecutionContext<'_>,
    generated_orders: Vec<MarginCallOrderRequest>,
    fill_processor: &mut P,
) -> Vec<OrderTicket> {
    if ctx.margin_remaining() >= Decimal::ZERO {
        return Vec::new();
    }

    let mut orders_with_pnl: Vec<(MarginCallOrderRequest, Decimal)> = generated_orders
        .into_iter()
        .map(|order| {
            let unrealized = ctx.portfolio.get_holding(&order.symbol).unrealized_pnl;
            (order, unrealized)
        })
        .collect();

    orders_with_pnl.sort_by_key(|(_, pnl)| *pnl);

    let mut executed = Vec::new();
    for (request, _) in orders_with_pnl {
        if ctx.margin_remaining() >= Decimal::ZERO {
            break;
        }

        let ticket = ctx.algorithm.market_order_with_options_and_tag(
            &request.symbol,
            request.quantity,
            None,
            false,
            request.tag,
        );
        let order_id = ticket.order_id;

        let mut events = ctx.order_processor.generate_order_events_with_quotes(
            ctx.trade_bars,
            ctx.quote_bars,
            ctx.time,
        );
        events.retain(|event| event.order_id == order_id);
        if !events.is_empty() {
            fill_processor.process_fills(&mut events);
        }

        ctx.refresh_total_margin_used();
        executed.push(ticket);
    }

    executed
}
