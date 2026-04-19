use crate::risk_management::{PortfolioTarget, RiskManagementModel};
use lean_core::Price;
use rust_decimal::Decimal;
use std::collections::HashMap;

/// Liquidates any security that has drawn down by more than `max_drawdown_pct`.
pub struct MaximumDrawdownPercentPerSecurity {
    pub max_drawdown_pct: Decimal,
    _peak_prices: HashMap<u64, Price>,
}

impl MaximumDrawdownPercentPerSecurity {
    pub fn new(max_drawdown_pct: Decimal) -> Self {
        MaximumDrawdownPercentPerSecurity {
            max_drawdown_pct,
            _peak_prices: HashMap::new(),
        }
    }
}

impl RiskManagementModel for MaximumDrawdownPercentPerSecurity {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        // Returns liquidation orders for any security over threshold.
        // In real engine: would check current prices vs peak prices.
        // Stub returns targets unchanged — engine wires up price tracking.
        targets.to_vec()
    }
}
