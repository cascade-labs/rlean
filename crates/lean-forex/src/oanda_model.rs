use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// OANDA brokerage model — spread-based, no commission.
pub struct OandaBrokerageModel {
    pub default_leverage: Decimal,
    pub spread_bps: Decimal, // half-spread in basis points
}

impl Default for OandaBrokerageModel {
    fn default() -> Self {
        Self { default_leverage: dec!(50), spread_bps: dec!(1.5) }
    }
}

impl OandaBrokerageModel {
    pub fn effective_spread(&self, mid_price: Decimal) -> Decimal {
        mid_price * self.spread_bps / dec!(10000)
    }
    pub fn buy_price(&self, mid: Decimal) -> Decimal { mid + self.effective_spread(mid) / dec!(2) }
    pub fn sell_price(&self, mid: Decimal) -> Decimal { mid - self.effective_spread(mid) / dec!(2) }
    pub fn max_leverage(&self) -> Decimal { self.default_leverage }
}
