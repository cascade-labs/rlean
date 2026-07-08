use lean_alpha::{ActiveInsightSnapshot, IAlphaModel};
use lean_execution::IExecutionModel;
use lean_portfolio_construction::IPortfolioConstructionModel;
use lean_risk::risk_management::RiskManagementModel;
use std::sync::{Arc, Mutex};

/// Engine-owned framework registration surface exposed to language bindings.
pub trait FrameworkModelRegistry: Send + Sync {
    fn add_alpha_model(&self, model: Box<dyn IAlphaModel>);
    fn set_portfolio_construction_model(&self, model: Box<dyn IPortfolioConstructionModel>);
    fn set_execution_model(&self, model: Box<dyn IExecutionModel>);
    fn set_risk_management_model(&self, model: Box<dyn RiskManagementModel>);
    fn insight_snapshot(&self) -> Arc<Mutex<ActiveInsightSnapshot>>;
    fn ensure_insight_observer(&self);
}
