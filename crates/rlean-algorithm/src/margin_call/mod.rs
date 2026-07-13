pub mod context;
pub mod default_model;
pub mod execute;
pub mod model;
pub mod position_group;
pub mod process;

pub use context::{build_margin_call_context, MarginCallContext};
pub use default_model::DefaultMarginCallModel;
pub use execute::execute_margin_call_orders;
pub use model::{
    MarginCallExecutionContext, MarginCallModel, MarginCallModelKind, MarginCallOrderRequest,
    NullMarginCallModel,
};
pub use position_group::{DefaultPositionGroup, PositionGroupCollection};
pub use process::{
    check_backtest_bankruptcy, process_margin_call_scan, MarginCallFillProcessor,
    MarginCallScanOutcome,
};
