use lean_brokerages::Brokerage;
use lean_core::Symbol;
use lean_orders::OrderEvent;
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Debug, Clone, Default)]
pub struct BrokerageSyncState {
    pub cash: HashMap<String, Decimal>,
    pub holdings: HashMap<Symbol, Decimal>,
    pub order_events: Vec<OrderEvent>,
}

pub async fn sync_brokerage_state(
    brokerage: Option<&mut Box<dyn Brokerage>>,
) -> anyhow::Result<BrokerageSyncState> {
    let Some(_brokerage) = brokerage else {
        return Ok(BrokerageSyncState::default());
    };

    // The detailed brokerage synchronization path is moving out of
    // `lean-python::runner`; this engine-owned hook is the live runner boundary.
    Ok(BrokerageSyncState::default())
}
