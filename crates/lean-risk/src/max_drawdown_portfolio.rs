use crate::risk_management::{PortfolioTarget, RiskContext, RiskManagementModel};
use lean_core::Symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Liquidates ALL positions when the portfolio-level drawdown from its peak
/// exceeds the threshold.
///
/// Matches `MaximumDrawdownPercentPortfolio` from LEAN C#.
///
/// * `is_trailing = false` (default) — drawdown is measured from the initial
///   portfolio value captured on the first call.
/// * `is_trailing = true` — drawdown is measured from the running high-water mark.
///
/// Once triggered the model resets (mirrors C# `_initialised = false`) so the
/// algorithm can re-enter on the next rebalancing cycle.
pub struct MaximumDrawdownPercentPortfolio {
    pub maximum_drawdown_pct: Decimal,
    pub is_trailing: bool,
    portfolio_high: Decimal,
    initialized: bool,
}

impl MaximumDrawdownPercentPortfolio {
    pub fn new(maximum_drawdown_pct: Decimal, is_trailing: bool) -> Self {
        MaximumDrawdownPercentPortfolio {
            maximum_drawdown_pct: maximum_drawdown_pct.abs(),
            is_trailing,
            portfolio_high: Decimal::ZERO,
            initialized: false,
        }
    }

    fn drawdown_pct(&self, current_value: Decimal) -> Decimal {
        if self.portfolio_high.is_zero() {
            return Decimal::ZERO;
        }
        // (current / high) - 1  — negative when below high
        (current_value / self.portfolio_high) - dec!(1)
    }
}

impl RiskManagementModel for MaximumDrawdownPercentPortfolio {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        // Without context we cannot compute drawdown — pass through.
        targets.to_vec()
    }

    fn manage_risk_with_context(
        &mut self,
        targets: &[PortfolioTarget],
        ctx: &RiskContext,
    ) -> Vec<PortfolioTarget> {
        let current_value = ctx.total_portfolio_value;

        if !self.initialized {
            self.portfolio_high = current_value;
            self.initialized = true;
        }

        // Update trailing high-water mark.
        if self.is_trailing && current_value > self.portfolio_high {
            self.portfolio_high = current_value;
            // New high reached — nothing to liquidate.
            return Vec::new();
        }

        let pnl = self.drawdown_pct(current_value);

        // pnl is negative when below high; threshold is stored as positive.
        // Trigger when pnl < -threshold  (drawdown exceeded).
        if pnl < -self.maximum_drawdown_pct && !targets.is_empty() {
            // Reset so the algo can re-enter on the next cycle.
            self.initialized = false;

            // Emit liquidation targets for every symbol in the targets list.
            targets
                .iter()
                .map(|t| PortfolioTarget::new(t.symbol.clone(), Decimal::ZERO))
                .collect()
        } else {
            Vec::new()
        }
    }
}

/// Convenience: build liquidation targets for a list of symbols.
#[allow(dead_code)]
fn liquidate_symbols(symbols: &[Symbol]) -> Vec<PortfolioTarget> {
    symbols
        .iter()
        .map(|s| PortfolioTarget::new(s.clone(), Decimal::ZERO))
        .collect()
}
