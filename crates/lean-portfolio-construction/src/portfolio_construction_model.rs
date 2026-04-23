use crate::portfolio_target::PortfolioTarget;
use lean_core::Symbol;
use std::collections::HashMap;

/// Direction of an alpha insight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsightDirection {
    Up = 1,
    Flat = 0,
    Down = -1,
}

impl InsightDirection {
    /// Returns the integer sign: 1, 0, or -1.
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Minimal insight representation for portfolio construction.
/// Deliberately does NOT depend on lean-alpha to avoid circular deps.
#[derive(Debug, Clone)]
pub struct InsightForPcm {
    pub symbol: Symbol,
    pub direction: InsightDirection,
    /// Expected return magnitude (absolute value, e.g. 0.05 for 5%)
    pub magnitude: Option<rust_decimal::Decimal>,
    /// Confidence in the insight (0.0 to 1.0)
    pub confidence: Option<rust_decimal::Decimal>,
    /// Source model name (used for grouping in Black-Litterman style)
    pub source_model: String,
}

/// Converts alpha insights into portfolio targets.
/// Mirrors C# IPortfolioConstructionModel.
pub trait IPortfolioConstructionModel: Send + Sync {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: rust_decimal::Decimal,
        prices: &HashMap<String, rust_decimal::Decimal>,
    ) -> Vec<PortfolioTarget>;

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    /// Called every bar with current security prices, even when no insights are
    /// emitted by the alpha model.  Models that require a rolling price history
    /// (e.g. Black-Litterman, Mean-Variance) override this to accumulate data
    /// so their warm-up period runs concurrently with the alpha warm-up.
    fn update_security_prices(&mut self, _prices: &HashMap<String, rust_decimal::Decimal>) {}

    fn name(&self) -> &str {
        "PortfolioConstructionModel"
    }
}
