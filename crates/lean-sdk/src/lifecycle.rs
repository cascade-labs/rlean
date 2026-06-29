use lean_sdk_annotations::{sdk_bind, sdk_callback_adapter, sdk_method};

pub use crate::native_bridge::QcAlgorithmNativeBridge;

pub use lean_algorithm::lifecycle::{
    AlgorithmBridge, AlgorithmServices, AlgorithmStateAccess, LifecycleBridge,
    NoopAlgorithmServices, OptionSubscription, UniverseSelection,
};

/// Bindgen marker for generated Python lifecycle callback dispatch.
///
/// These methods intentionally mirror the snake_case lifecycle callback names
/// that user strategies implement. The generated Python adapter uses this as
/// the source of truth for method names; engine ordering remains in
/// `lean-engine::algorithm_manager`.
#[sdk_bind(py_name = "LifecycleCallbacks")]
#[sdk_callback_adapter]
pub struct LifecycleCallbacks;

impl LifecycleCallbacks {
    #[sdk_method]
    pub fn initialize(&self) {}

    #[sdk_method]
    pub fn on_data(&self) {}

    #[sdk_method]
    pub fn on_order_event(&self) {}

    #[sdk_method]
    pub fn on_assignment_order_event(&self) {}

    #[sdk_method]
    pub fn on_end_of_day(&self) {}

    #[sdk_method]
    pub fn on_warmup_finished(&self) {}

    #[sdk_method]
    pub fn on_end_of_algorithm(&self) {}

    #[sdk_method]
    pub fn on_margin_call(&self) {}

    #[sdk_method]
    pub fn on_margin_call_warning(&self) {}

    #[sdk_method]
    pub fn on_securities_changed(&self) {}

    #[sdk_method]
    pub fn on_splits(&self) {}

    #[sdk_method]
    pub fn on_dividends(&self) {}

    #[sdk_method]
    pub fn on_delistings(&self) {}

    #[sdk_method]
    pub fn on_symbol_changed_events(&self) {}

    #[sdk_method]
    pub fn on_framework_data(&self) {}

    #[sdk_method]
    pub fn on_end_of_time_step(&self) {}

    #[sdk_method]
    pub fn on_brokerage_message(&self) {}

    #[sdk_method]
    pub fn on_brokerage_disconnect(&self) {}

    #[sdk_method]
    pub fn on_brokerage_reconnect(&self) {}
}
