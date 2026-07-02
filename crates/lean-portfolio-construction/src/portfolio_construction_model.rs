use crate::portfolio_target::PortfolioTarget;
use lean_core::{Symbol, TimeSpan};
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

/// Borrowed insight representation for engine hot paths.
///
/// The default trait adapter materializes owned [`InsightForPcm`] values for
/// existing models. Models that care about allocation can override
/// `create_targets_from_refs` directly.
#[derive(Debug, Clone, Copy)]
pub struct InsightForPcmRef<'a> {
    pub symbol: &'a Symbol,
    pub direction: InsightDirection,
    pub magnitude: Option<rust_decimal::Decimal>,
    pub confidence: Option<rust_decimal::Decimal>,
    pub source_model: &'a str,
}

/// Converts alpha insights into portfolio targets.
/// Mirrors C# IPortfolioConstructionModel.
pub trait IPortfolioConstructionModel: Send + Sync {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: rust_decimal::Decimal,
        prices: &HashMap<u64, rust_decimal::Decimal>,
    ) -> Vec<PortfolioTarget>;

    fn create_targets_from_refs(
        &mut self,
        insights: &[InsightForPcmRef<'_>],
        portfolio_value: rust_decimal::Decimal,
        prices: &HashMap<u64, rust_decimal::Decimal>,
    ) -> Vec<PortfolioTarget> {
        let owned: Vec<InsightForPcm> = insights
            .iter()
            .map(|insight| InsightForPcm {
                symbol: insight.symbol.clone(),
                direction: insight.direction,
                magnitude: insight.magnitude,
                confidence: insight.confidence,
                source_model: insight.source_model.to_string(),
            })
            .collect();
        self.create_targets(&owned, portfolio_value, prices)
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    /// Called every bar with current security prices, even when no insights are
    /// emitted by the alpha model.  Models that require a rolling price history
    /// (e.g. Black-Litterman, Mean-Variance) override this to accumulate data
    /// so their warm-up period runs concurrently with the alpha warm-up.
    fn update_security_prices(&mut self, _prices: &HashMap<u64, rust_decimal::Decimal>) {}

    /// Rebalance frequency for models that follow LEAN's scheduled PCM behavior.
    /// `None` preserves the legacy rlean behavior of creating targets whenever
    /// active insights are present.
    fn rebalance_period(&self) -> Option<TimeSpan> {
        None
    }

    fn rebalance_on_security_changes(&self) -> bool {
        true
    }

    fn rebalance_on_insight_changes(&self) -> bool {
        true
    }

    /// Whether this PCM can consume multiple active insights for the same
    /// symbol. Models such as Black-Litterman use source-model groups as
    /// distinct investor views, so collapsing to one insight per symbol loses
    /// the alpha ensemble before optimization.
    fn use_all_active_insights(&self) -> bool {
        false
    }

    fn name(&self) -> &str {
        "PortfolioConstructionModel"
    }
}
