use crate::risk_management::{PortfolioTarget, RiskContext, RiskManagementModel};
use lean_core::Symbol;
use rust_decimal::Decimal;

/// Liquidates any security that has drawn down by more than `max_drawdown_pct`.
pub struct MaximumDrawdownPercentPerSecurity {
    pub max_drawdown_pct: Decimal,
    canceled_symbols: Vec<Symbol>,
}

impl MaximumDrawdownPercentPerSecurity {
    pub fn new(max_drawdown_pct: Decimal) -> Self {
        MaximumDrawdownPercentPerSecurity {
            max_drawdown_pct,
            canceled_symbols: Vec::new(),
        }
    }
}

impl RiskManagementModel for MaximumDrawdownPercentPerSecurity {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        targets.to_vec()
    }

    fn manage_risk_with_context(
        &mut self,
        _targets: &[PortfolioTarget],
        ctx: &RiskContext,
    ) -> Vec<PortfolioTarget> {
        let maximum_drawdown = -self.max_drawdown_pct.abs();
        self.canceled_symbols.clear();
        let mut result = Vec::new();

        for holding in &ctx.holdings {
            if !holding.is_invested() {
                continue;
            }

            if holding.unrealized_profit_pct() < maximum_drawdown {
                self.canceled_symbols.push(holding.symbol.clone());
                result.push(PortfolioTarget::new(holding.symbol.clone(), Decimal::ZERO));
            }
        }

        result
    }

    fn canceled_insights(&mut self) -> Vec<Symbol> {
        std::mem::take(&mut self.canceled_symbols)
    }
}
