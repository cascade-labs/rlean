use crate::risk_management::{PortfolioTarget, RiskManagementModel};
use lean_core::Price;
use rust_decimal::Decimal;
use std::collections::HashMap;

pub struct TrailingStopRiskManagementModel {
    pub trailing_pct: Decimal,
    high_prices: HashMap<u64, Price>,
    low_prices: HashMap<u64, Price>,
}

impl TrailingStopRiskManagementModel {
    pub fn new(trailing_pct: Decimal) -> Self {
        TrailingStopRiskManagementModel {
            trailing_pct,
            high_prices: HashMap::new(),
            low_prices: HashMap::new(),
        }
    }
}

impl RiskManagementModel for TrailingStopRiskManagementModel {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        targets.to_vec()
    }
}
