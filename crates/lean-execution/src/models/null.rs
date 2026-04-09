use std::collections::HashMap;

use crate::execution_model::{ExecutionTarget, IExecutionModel, OrderRequest, SecurityData};
use lean_core::Symbol;

/// Null execution model — never submits any orders.
///
/// Useful as a placeholder or for testing.
pub struct NullExecutionModel;

impl NullExecutionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NullExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IExecutionModel for NullExecutionModel {
    fn execute(
        &mut self,
        _targets: &[ExecutionTarget],
        _securities: &HashMap<String, SecurityData>,
    ) -> Vec<OrderRequest> {
        vec![]
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    fn name(&self) -> &str {
        "NullExecutionModel"
    }
}
