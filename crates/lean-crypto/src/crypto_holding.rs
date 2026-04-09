use lean_core::Symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone)]
pub struct CryptoHolding {
    pub symbol: Symbol,
    pub quantity: Decimal,
    pub average_price: Decimal,
    pub leverage: Decimal,
    pub unrealized_pnl: Decimal,
    pub funding_pnl: Decimal,
    pub liquidation_price: Option<Decimal>,
}

impl CryptoHolding {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            quantity: dec!(0),
            average_price: dec!(0),
            leverage: dec!(1),
            unrealized_pnl: dec!(0),
            funding_pnl: dec!(0),
            liquidation_price: None,
        }
    }

    pub fn update_price(&mut self, mark_price: Decimal) {
        if self.average_price.is_zero() || self.quantity.is_zero() {
            self.unrealized_pnl = dec!(0);
            return;
        }
        self.unrealized_pnl = (mark_price - self.average_price) * self.quantity;
    }

    pub fn apply_funding(&mut self, payment: Decimal) {
        self.funding_pnl += payment;
    }

    pub fn is_invested(&self) -> bool {
        !self.quantity.is_zero()
    }

    pub fn is_long(&self) -> bool {
        self.quantity > dec!(0)
    }

    pub fn is_short(&self) -> bool {
        self.quantity < dec!(0)
    }
}
