use rlean_alpha::{ActiveInsightSnapshot, IAlphaModel};
use rlean_core::{DateTime, Symbol};
use rlean_execution::IExecutionModel;
use rlean_portfolio_construction::IPortfolioConstructionModel;
use rlean_risk::risk_management::RiskManagementModel;
use std::sync::{Arc, Mutex};

/// Engine-owned framework registration surface exposed to language bindings.
pub trait FrameworkModelRegistry: Send + Sync {
    fn add_alpha_model(&self, model: Box<dyn IAlphaModel>);
    fn set_portfolio_construction_model(&self, model: Box<dyn IPortfolioConstructionModel>);
    fn set_execution_model(&self, model: Box<dyn IExecutionModel>);
    fn set_risk_management_model(&self, model: Box<dyn RiskManagementModel>);
    fn insight_snapshot(&self) -> Arc<Mutex<ActiveInsightSnapshot>>;
    /// Cancel the active insights for `symbols` at `utc_now`.
    ///
    /// Mirrors C# LEAN's `Algorithm.Insights.Cancel(...)`. Strategies that
    /// liquidate while using the Algorithm Framework must cancel the active
    /// insight as well, otherwise the next PCM pass is allowed to recreate the
    /// position.
    fn cancel_insights(&self, symbols: &[Symbol], utc_now: DateTime);
    fn ensure_insight_observer(&self);
}
