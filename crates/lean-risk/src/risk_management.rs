use lean_core::{Price, Symbol};
use rust_decimal::Decimal;

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

/// Snapshot of a single holding passed into risk models that need per-security data.
#[derive(Debug, Clone)]
pub struct HoldingSnapshot {
    pub symbol: Symbol,
    pub quantity: Decimal,
    pub average_price: Decimal,
    pub last_price: Decimal,
    pub unrealized_pnl: Decimal,
}

impl HoldingSnapshot {
    /// Unrealized profit % for a long position: (last - avg) / avg.
    /// Returns 0 if average_price is zero.
    pub fn unrealized_profit_pct(&self) -> Decimal {
        if self.average_price.is_zero() {
            return Decimal::ZERO;
        }
        (self.last_price - self.average_price) / self.average_price
    }

    pub fn is_invested(&self) -> bool {
        !self.quantity.is_zero()
    }
}

/// Context passed to risk models that need portfolio-level or per-security data.
#[derive(Debug, Clone, Default)]
pub struct RiskContext {
    /// Current total portfolio value (cash + market value of all positions).
    pub total_portfolio_value: Decimal,
    /// All current invested holdings.
    pub holdings: Vec<HoldingSnapshot>,
}

pub trait RiskManagementModel: Send + Sync {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget>;

    /// Called with full portfolio context.  Default impl ignores context and
    /// delegates to `manage_risk`, so existing impls need not override this.
    fn manage_risk_with_context(
        &mut self,
        targets: &[PortfolioTarget],
        _ctx: &RiskContext,
    ) -> Vec<PortfolioTarget> {
        self.manage_risk(targets)
    }
}

pub struct NullRiskManagement;

impl RiskManagementModel for NullRiskManagement {
    fn manage_risk(&mut self, targets: &[PortfolioTarget]) -> Vec<PortfolioTarget> {
        targets.to_vec()
    }
}
