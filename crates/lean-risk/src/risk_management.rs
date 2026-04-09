use lean_core::{Price, Symbol};

#[derive(Debug, Clone)]
pub struct PortfolioTarget {
    pub symbol: Symbol,
    pub quantity: Price,
}

impl PortfolioTarget {
    pub fn new(symbol: Symbol, quantity: Price) -> Self {
        PortfolioTarget { symbol, quantity }
    }
}

pub trait RiskManagementModel: Send + Sync {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget>;
}

pub struct NullRiskManagement;

impl RiskManagementModel for NullRiskManagement {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        targets.to_vec()
    }
}
