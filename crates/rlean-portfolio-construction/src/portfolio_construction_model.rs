use crate::portfolio_target::PortfolioTarget;
use rlean_core::{DateTime, Symbol, TimeSpan};
use std::collections::HashMap;
use std::sync::Arc;

/// Portfolio rebalance cadence and change-trigger settings.
///
/// This is the Rust-owned equivalent of LEAN's `PortfolioConstructionModel`
/// rebalancing function plus `RebalanceOn*Changes` switches. The engine owns the
/// runtime next-rebalance state; portfolio construction models only declare the
/// policy they want.
#[derive(Clone)]
pub struct RebalancePolicy {
    cadence: RebalanceCadence,
    rebalance_on_security_changes: bool,
    rebalance_on_insight_changes: bool,
}

/// Scheduled rebalance cadence.
#[derive(Clone)]
pub enum RebalanceCadence {
    /// No scheduled next time. Rebalance whenever the framework has targets to
    /// process, matching C# LEAN's null rebalancing function behavior.
    EverySlice,
    /// Rebalance after a fixed elapsed period from the last refresh.
    Period(TimeSpan),
    /// For a given UTC time, return the next expected rebalance time. Returning
    /// `None` means the next time is unknown and should be requested again on
    /// the next framework loop, matching C# LEAN's nullable rebalancing func.
    NextTime(Arc<dyn Fn(DateTime) -> Option<DateTime> + Send + Sync>),
}

impl RebalancePolicy {
    pub fn every_slice() -> Self {
        Self {
            cadence: RebalanceCadence::EverySlice,
            rebalance_on_security_changes: true,
            rebalance_on_insight_changes: true,
        }
    }

    pub fn period(period: TimeSpan) -> Self {
        Self {
            cadence: RebalanceCadence::Period(period),
            rebalance_on_security_changes: true,
            rebalance_on_insight_changes: true,
        }
    }

    pub fn daily() -> Self {
        Self::period(TimeSpan::ONE_DAY)
    }

    pub fn next_time(
        next_time: impl Fn(DateTime) -> Option<DateTime> + Send + Sync + 'static,
    ) -> Self {
        Self {
            cadence: RebalanceCadence::NextTime(Arc::new(next_time)),
            rebalance_on_security_changes: true,
            rebalance_on_insight_changes: true,
        }
    }

    /// Rebalance only when the active insight set changes.
    ///
    /// The scheduler deliberately returns no next time, and security changes do
    /// not trigger target generation. New and expired insights still do. This is
    /// the provider- and language-neutral equivalent of LEAN's nullable
    /// rebalancing function returning `None` with
    /// `RebalanceOnSecurityChanges = false`.
    pub fn insight_changes_only() -> Self {
        Self::next_time(|_| None).with_security_changes(false)
    }

    pub fn from_period(period: Option<TimeSpan>) -> Self {
        period.map(Self::period).unwrap_or_else(Self::every_slice)
    }

    pub fn cadence(&self) -> &RebalanceCadence {
        &self.cadence
    }

    pub fn rebalance_on_security_changes(&self) -> bool {
        self.rebalance_on_security_changes
    }

    pub fn rebalance_on_insight_changes(&self) -> bool {
        self.rebalance_on_insight_changes
    }

    pub fn with_security_changes(mut self, enabled: bool) -> Self {
        self.rebalance_on_security_changes = enabled;
        self
    }

    pub fn with_insight_changes(mut self, enabled: bool) -> Self {
        self.rebalance_on_insight_changes = enabled;
        self
    }

    pub fn period_value(&self) -> Option<TimeSpan> {
        match self.cadence {
            RebalanceCadence::Period(period) => Some(period),
            RebalanceCadence::EverySlice | RebalanceCadence::NextTime(_) => None,
        }
    }
}

impl Default for RebalancePolicy {
    fn default() -> Self {
        Self::daily()
    }
}

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
/// Deliberately does NOT depend on rlean-alpha to avoid circular deps.
#[derive(Debug, Clone)]
pub struct InsightForPcm {
    pub symbol: Symbol,
    pub direction: InsightDirection,
    /// Expected return magnitude (absolute value, e.g. 0.05 for 5%)
    pub magnitude: Option<rust_decimal::Decimal>,
    /// Confidence in the insight (0.0 to 1.0)
    pub confidence: Option<rust_decimal::Decimal>,
    /// Portfolio allocation weight (0.0 to 1.0).
    pub weight: Option<rust_decimal::Decimal>,
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
    pub weight: Option<rust_decimal::Decimal>,
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
                weight: insight.weight,
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

    /// Default rebalance cadence for a portfolio construction model.
    ///
    /// Mirrors C# LEAN's base `PortfolioConstructionModel`, whose default
    /// `rebalancingFunc` is `null`. In `IsRebalanceDue` a null func returns
    /// `true` every call, i.e. the model rebalances on **every slice**. Built-in
    /// models (e.g. `EqualWeightingPortfolioConstructionModel`, which defaults to
    /// `Resolution.Daily`) override this with their own cadence, but a bare
    /// custom PCM — including Python subclasses of `PortfolioConstructionModel`
    /// that never set a rebalancing function — must rebalance every bar to match
    /// LEAN. Returning `daily()` here silently throttled custom PCMs to one
    /// rebalance per day.
    fn rebalance_policy(&self) -> RebalancePolicy {
        RebalancePolicy::every_slice()
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
