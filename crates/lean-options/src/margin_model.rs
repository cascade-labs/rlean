use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use crate::contract::OptionContract;

pub struct OptionMarginModel {
    /// Naked position margin requirement (default 10%)
    pub naked_margin: Decimal,
    /// OTM equity naked position additional requirement (default 20%)
    pub otm_equity_naked_margin: Decimal,
}

impl Default for OptionMarginModel {
    fn default() -> Self {
        OptionMarginModel {
            naked_margin: dec!(0.10),
            otm_equity_naked_margin: dec!(0.20),
        }
    }
}

impl OptionMarginModel {
    /// Initial margin requirement for a position.
    /// Long options: 0 (premium already paid).
    /// Short options: based on OTM/ITM and position type.
    pub fn initial_margin(&self, contract: &OptionContract, quantity: Decimal, underlying_price: Decimal) -> Decimal {
        if quantity >= Decimal::ZERO {
            // Long: margin = 0 (premium paid upfront)
            return Decimal::ZERO;
        }
        let position_value = underlying_price
            * Decimal::from(contract.contract_multiplier)
            * quantity.abs();
        position_value * self.naked_margin
    }

    /// Maintenance margin for an existing short position.
    pub fn maintenance_margin(&self, contract: &OptionContract, quantity: Decimal, cost_basis: Decimal) -> Decimal {
        if quantity >= Decimal::ZERO { return Decimal::ZERO; }
        let _ = contract; // contract reserved for future use
        cost_basis.abs() * self.naked_margin
    }
}
