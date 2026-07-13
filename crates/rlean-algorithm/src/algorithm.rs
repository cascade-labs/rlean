use rlean_core::Result as LeanResult;
use rlean_data::Slice;
use rlean_orders::OrderEvent;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlgorithmStatus {
    Initializing,
    History,
    Running,
    Stopped,
    Liquidated,
    Deleted,
    Completed,
    RuntimeError,
    Invalid,
    LoggingIn,
    Warming,
    DeployError,
}

/// Engine-owned payload for language-adapter slice delivery.
pub struct DataDeliveryPayload {
    pub slice: Arc<Slice>,
}

/// Standard Rust strategy surface backed by shared `QcAlgorithm` state.
///
/// This is the native equivalent of Python subclassing `QCAlgorithm`: the
/// strategy owns callbacks, while engine services and mutable algorithm state
/// live in the shared `QcAlgorithm` core.
pub trait QcAlgorithmStrategy: Send + Sync {
    fn algorithm_state(&self) -> Arc<Mutex<crate::qc_algorithm::QcAlgorithm>>;

    fn initialize(&mut self) -> LeanResult<()>;

    fn on_data(&mut self, _payload: DataDeliveryPayload) {}

    fn on_order_event(&mut self, _order_event: &OrderEvent) {}

    fn on_end_of_day(&mut self, _symbol: Option<rlean_core::Symbol>) {}

    fn on_warmup_finished(&mut self) {}

    fn on_end_of_algorithm(&mut self) {}

    fn on_margin_call(&mut self, _requests: &[rlean_orders::Order]) {}

    fn on_margin_call_warning(&mut self) {}

    fn on_securities_changed(&mut self, _changes: &SecurityChanges) {}
}

/// Security additions/removals from universe changes.
#[derive(Debug, Clone)]
pub struct SecurityChanges {
    pub added: Vec<rlean_core::Symbol>,
    pub removed: Vec<rlean_core::Symbol>,
}

impl SecurityChanges {
    pub fn empty() -> Self {
        SecurityChanges {
            added: vec![],
            removed: vec![],
        }
    }

    pub fn has_changes(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}
