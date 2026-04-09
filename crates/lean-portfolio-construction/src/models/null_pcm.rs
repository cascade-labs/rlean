use std::collections::HashMap;
use rust_decimal::Decimal;
use lean_core::Symbol;

use crate::portfolio_construction_model::{IPortfolioConstructionModel, InsightForPcm};
use crate::portfolio_target::PortfolioTarget;

/// A no-op portfolio construction model that returns no targets.
/// Useful as a placeholder or for pass-through scenarios.
pub struct NullPortfolioConstructionModel;

impl NullPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NullPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for NullPortfolioConstructionModel {
    fn create_targets(
        &mut self,
        _insights: &[InsightForPcm],
        _portfolio_value: Decimal,
        _prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        vec![]
    }

    fn name(&self) -> &str {
        "NullPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}
