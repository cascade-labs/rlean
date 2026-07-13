use rust_decimal::Decimal;

use crate::margin_call::position_group::PositionGroupCollection;
use crate::portfolio::SecurityPortfolioManager;
use crate::qc_algorithm::QcAlgorithm;
use std::sync::Arc;

/// Read-only inputs for margin call scanning.
pub struct MarginCallContext {
    pub portfolio: Arc<SecurityPortfolioManager>,
    pub total_portfolio_value: Decimal,
    pub total_margin_used: Decimal,
    pub groups: PositionGroupCollection,
}

impl MarginCallContext {
    pub fn margin_remaining(&self) -> Decimal {
        self.portfolio
            .margin_remaining_for_value(self.total_portfolio_value, self.total_margin_used)
    }
}

pub fn build_margin_call_context(
    portfolio: &Arc<SecurityPortfolioManager>,
    algorithm: &QcAlgorithm,
) -> MarginCallContext {
    MarginCallContext {
        portfolio: Arc::clone(portfolio),
        total_portfolio_value: portfolio.total_portfolio_value(),
        total_margin_used: algorithm.total_margin_used(),
        groups: PositionGroupCollection::from_holdings(
            &portfolio.all_holdings(),
            &algorithm.securities,
        ),
    }
}
