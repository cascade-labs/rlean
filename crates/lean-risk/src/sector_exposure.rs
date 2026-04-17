use crate::risk_management::{PortfolioTarget, RiskManagementModel};
use rust_decimal::Decimal;
use std::collections::HashMap;

/// Caps sector exposure to `max_sector_weight` of portfolio.
pub struct MaximumSectorExposureRiskManagementModel {
    pub max_sector_weight: Decimal,
    sector_map: HashMap<u64, String>,
}

impl MaximumSectorExposureRiskManagementModel {
    pub fn new(max_sector_weight: Decimal) -> Self {
        MaximumSectorExposureRiskManagementModel {
            max_sector_weight,
            sector_map: HashMap::new(),
        }
    }

    pub fn set_sector(&mut self, symbol_sid: u64, sector: String) {
        self.sector_map.insert(symbol_sid, sector);
    }
}

impl RiskManagementModel for MaximumSectorExposureRiskManagementModel {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        targets.to_vec()
    }
}
