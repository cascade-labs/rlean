use lean_core::{CurrencyPair, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Forex-specific position tracking.
#[derive(Debug, Clone)]
pub struct ForexHolding {
    pub symbol: Symbol,
    pub currency_pair: CurrencyPair,
    pub quantity: Decimal,       // in base currency units
    pub average_price: Decimal,  // average entry price
    pub unrealized_pnl: Decimal, // in quote currency
    pub swap_pnl: Decimal,       // accumulated overnight swap
}

impl ForexHolding {
    pub fn new(symbol: Symbol, currency_pair: CurrencyPair) -> Self {
        Self {
            symbol,
            currency_pair,
            quantity: dec!(0),
            average_price: dec!(0),
            unrealized_pnl: dec!(0),
            swap_pnl: dec!(0),
        }
    }

    pub fn update_price(&mut self, current_price: Decimal) {
        if self.average_price.is_zero() || self.quantity.is_zero() {
            self.unrealized_pnl = dec!(0);
            return;
        }
        self.unrealized_pnl = (current_price - self.average_price) * self.quantity;
    }

    pub fn apply_swap(&mut self, swap_amount: Decimal) {
        self.swap_pnl += swap_amount;
    }

    pub fn total_pnl(&self) -> Decimal {
        self.unrealized_pnl + self.swap_pnl
    }
}
