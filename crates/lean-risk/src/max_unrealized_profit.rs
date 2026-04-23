use crate::risk_management::{PortfolioTarget, RiskContext, RiskManagementModel};
use rust_decimal::Decimal;

/// Liquidates a security when its unrealized profit exceeds the threshold
/// (take-profit logic).
///
/// Matches `MaximumUnrealizedProfitPercentPerSecurity` from LEAN C#.
///
/// * Unrealized profit % = (last_price - avg_cost) / avg_cost for long positions.
/// * For short positions the formula inverts:  (avg_cost - last_price) / avg_cost.
/// * Only invested securities are checked.
pub struct MaximumUnrealizedProfitPercentPerSecurity {
    pub maximum_unrealized_profit_pct: Decimal,
}

impl MaximumUnrealizedProfitPercentPerSecurity {
    pub fn new(maximum_unrealized_profit_pct: Decimal) -> Self {
        MaximumUnrealizedProfitPercentPerSecurity {
            maximum_unrealized_profit_pct: maximum_unrealized_profit_pct.abs(),
        }
    }
}

impl RiskManagementModel for MaximumUnrealizedProfitPercentPerSecurity {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        // Without holdings context we cannot compute unrealized profit — pass through.
        targets.to_vec()
    }

    fn manage_risk_with_context(
        &mut self,
        _targets: &[PortfolioTarget],
        ctx: &RiskContext,
    ) -> Vec<PortfolioTarget> {
        let mut result = Vec::new();

        for holding in &ctx.holdings {
            if !holding.is_invested() {
                continue;
            }

            // Unrealized profit %: sign-aware so both longs and shorts work.
            let pnl_pct = if holding.average_price.is_zero() {
                Decimal::ZERO
            } else if holding.quantity > Decimal::ZERO {
                // Long: profit when last > avg
                (holding.last_price - holding.average_price) / holding.average_price
            } else {
                // Short: profit when last < avg
                (holding.average_price - holding.last_price) / holding.average_price
            };

            if pnl_pct > self.maximum_unrealized_profit_pct {
                result.push(PortfolioTarget::new(holding.symbol.clone(), Decimal::ZERO));
            }
        }

        result
    }
}
