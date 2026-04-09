use lean_core::Symbol;
use rust_decimal::Decimal;

/// Target allocation for a single security. Mirrors C# PortfolioTarget.
#[derive(Debug, Clone)]
pub struct PortfolioTarget {
    pub symbol: Symbol,
    /// Target quantity (positive = long, negative = short, 0 = liquidate)
    pub quantity: Decimal,
    /// Optional target percentage of portfolio (0.0 to 1.0)
    pub percent: Option<Decimal>,
    pub tag: String,
}

impl PortfolioTarget {
    pub fn new(symbol: Symbol, quantity: Decimal) -> Self {
        Self {
            symbol,
            quantity,
            percent: None,
            tag: String::new(),
        }
    }

    /// Create a target from a target portfolio percentage.
    /// Computes quantity = round(portfolio_value * pct / price).
    pub fn percent(symbol: Symbol, pct: Decimal, portfolio_value: Decimal, price: Decimal) -> Self {
        let quantity = if price.is_zero() {
            Decimal::ZERO
        } else {
            (portfolio_value * pct / price).round()
        };
        Self {
            symbol,
            quantity,
            percent: Some(pct),
            tag: String::new(),
        }
    }
}
