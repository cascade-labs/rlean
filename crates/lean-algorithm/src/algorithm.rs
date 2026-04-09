use lean_core::{DateTime, Price, Result as LeanResult};
use lean_data::Slice;
use lean_orders::OrderEvent;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

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

/// Core algorithm interface — implement this to create a strategy.
pub trait IAlgorithm: Send + Sync {
    /// Called once at startup. Subscribe to data, set cash, configure settings.
    fn initialize(&mut self) -> LeanResult<()>;

    /// Called at the start of each day (if algorithm is running live or backtest).
    fn on_warmup_finished(&mut self) {}

    /// The main event handler. Called for each time step with available data.
    fn on_data(&mut self, slice: &Slice);

    /// Called when an order is filled, partially filled, or canceled.
    fn on_order_event(&mut self, order_event: &OrderEvent) {}

    /// Called at end of day.
    fn on_end_of_day(&mut self, _symbol: Option<lean_core::Symbol>) {}

    /// Called at end of algorithm run. Last chance to compute final stats.
    fn on_end_of_algorithm(&mut self) {}

    /// Called when margin call occurs.
    fn on_margin_call(&mut self, _requests: &[lean_orders::Order]) {}

    /// Called when securities change in universe.
    fn on_securities_changed(&mut self, _changes: &SecurityChanges) {}

    fn name(&self) -> &str;
    fn start_date(&self) -> DateTime;
    fn end_date(&self) -> DateTime;
    fn status(&self) -> AlgorithmStatus;

    /// Current total portfolio value (cash + holdings market value).
    /// Defaults to 0 — implementors backed by QcAlgorithm should override.
    fn portfolio_value(&self) -> Price { dec!(0) }

    /// Starting cash set during initialize().
    /// Defaults to 100,000 — implementors backed by QcAlgorithm should override.
    fn starting_cash(&self) -> Price { dec!(100_000) }
}

/// Security additions/removals from universe changes.
#[derive(Debug, Clone)]
pub struct SecurityChanges {
    pub added: Vec<lean_core::Symbol>,
    pub removed: Vec<lean_core::Symbol>,
}

impl SecurityChanges {
    pub fn empty() -> Self {
        SecurityChanges { added: vec![], removed: vec![] }
    }

    pub fn has_changes(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty()
    }
}
