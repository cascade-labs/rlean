use rlean_core::Symbol;
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
        Self::new_with_tag(symbol, quantity, "")
    }

    pub fn new_with_tag(symbol: Symbol, quantity: Decimal, tag: impl Into<String>) -> Self {
        Self {
            symbol,
            quantity,
            percent: None,
            tag: tag.into(),
        }
    }

    /// Create a target from a target portfolio percentage.
    ///
    /// `portfolio_value` is expected to have LEAN's free-portfolio buffer
    /// removed by the engine. Long targets round down and short targets round
    /// up so the resulting position is always at or just below its allocation.
    pub fn percent(symbol: Symbol, pct: Decimal, portfolio_value: Decimal, price: Decimal) -> Self {
        Self::percent_with_tag(symbol, pct, portfolio_value, price, "")
    }

    pub fn percent_with_tag(
        symbol: Symbol,
        pct: Decimal,
        portfolio_value: Decimal,
        price: Decimal,
        tag: impl Into<String>,
    ) -> Self {
        let quantity = if price.is_zero() {
            Decimal::ZERO
        } else {
            let raw = portfolio_value * pct / price;
            if raw.is_sign_negative() {
                raw.ceil()
            } else {
                raw.floor()
            }
        };
        Self {
            symbol,
            quantity,
            percent: Some(pct),
            tag: tag.into(),
        }
    }
}
