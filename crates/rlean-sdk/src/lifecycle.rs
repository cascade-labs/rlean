pub use crate::native_bridge::QcAlgorithmNativeBridge;

pub use rlean_algorithm::lifecycle::{
    AlgorithmBridge, AlgorithmServices, AlgorithmStateAccess, LifecycleBridge,
    NoopAlgorithmServices, OptionSubscription, UniverseSelection,
};

/// Bindgen marker for generated Python lifecycle callback dispatch.
///
/// These methods intentionally mirror the snake_case lifecycle callback names
/// that user strategies implement. The generated Python adapter uses this as
/// the source of truth for method names; engine ordering remains in
/// `rlean-engine::algorithm_manager`.
#[cfg_attr(feature = "python", pyo3::pyclass(name = "LifecycleCallbacks"))]
pub struct LifecycleCallbacks;

impl LifecycleCallbacks {
    pub fn initialize(&self) {}

    pub fn on_data(&self) {}

    pub fn on_order_event(&self) {}

    pub fn on_assignment_order_event(&self) {}

    pub fn on_end_of_day(&self) {}

    pub fn on_warmup_finished(&self) {}

    pub fn on_end_of_algorithm(&self) {}

    pub fn on_margin_call(&self) {}

    pub fn on_margin_call_warning(&self) {}

    pub fn on_securities_changed(&self) {}

    pub fn on_splits(&self) {}

    pub fn on_dividends(&self) {}

    pub fn on_delistings(&self) {}

    pub fn on_symbol_changed_events(&self) {}

    pub fn on_framework_data(&self) {}

    pub fn on_end_of_time_step(&self) {}

    pub fn on_brokerage_message(&self) {}

    pub fn on_brokerage_disconnect(&self) {}

    pub fn on_brokerage_reconnect(&self) {}
}
