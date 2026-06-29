use lean_algorithm::lifecycle::AlgorithmBridge;
use lean_algorithm::portfolio::{SecurityHolding, SecurityPortfolioManager};
use lean_orders::{OrderEvent, OrderProcessor};
use lean_statistics::{Trade, TradeBuilder};
use rust_decimal::Decimal;
use std::sync::Arc;

/// Sidecar persistence hook for order events and trades produced during an
/// engine-owned time step.
pub trait OrderEventSidecarWriter {
    fn append_order_events(&self, events: &[OrderEvent]);
    fn append_trades(&self, trades: &[Trade]);
}

pub fn as_sidecar_writer<T: OrderEventSidecarWriter>(
    writer: Option<&T>,
) -> Option<&dyn OrderEventSidecarWriter> {
    writer.map(|writer| writer as &dyn OrderEventSidecarWriter)
}

/// Settle a fill event using a language-neutral `AlgorithmBridge`.
pub fn settle_fill_event_bridge<B: AlgorithmBridge + ?Sized>(
    bridge: &B,
    portfolio: &Arc<SecurityPortfolioManager>,
    order_processor: &OrderProcessor,
    event: &mut OrderEvent,
) -> Option<Decimal> {
    if !event.is_fill() {
        return None;
    }
    let order = order_processor
        .transaction_manager
        .get_order(event.order_id)?;
    let fee = bridge.order_fee(&order, event.fill_price);
    let contract_multiplier = bridge.contract_multiplier_for_symbol(&order.symbol);
    if let Err(message) = bridge.validate_order_buying_power(&order, event.fill_price, fee) {
        *event = OrderEvent::invalid(order.id, order.symbol.clone(), event.utc_time, message);
        return None;
    }
    portfolio.apply_fill_with_multiplier(
        &order.symbol,
        event.fill_price,
        event.fill_quantity,
        fee,
        contract_multiplier,
    );
    event.order_fee = fee;
    Some(fee)
}

pub fn record_trade_fill(
    trade_builder: &mut TradeBuilder,
    completed_trades: &mut Vec<Trade>,
    event: &OrderEvent,
    fees: Decimal,
) -> Option<Trade> {
    let trade = trade_builder.record_fill(
        &event.symbol,
        event.utc_time,
        event.fill_price,
        event.fill_quantity,
        SecurityHolding::infer_contract_multiplier(&event.symbol),
        fees,
    )?;
    completed_trades.push(trade.clone());
    Some(trade)
}
