use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub struct FxcmBrokerageModel {
    pub default_leverage: Decimal,
    pub commission_per_lot: Decimal, // per 100k units
}

impl Default for FxcmBrokerageModel {
    fn default() -> Self {
        Self { default_leverage: dec!(100), commission_per_lot: dec!(4) } // $4/lot roundtrip
    }
}

impl FxcmBrokerageModel {
    pub fn commission(&self, quantity_units: Decimal) -> Decimal {
        let lots = quantity_units.abs() / Decimal::from(super::pip::STANDARD_LOT);
        lots * self.commission_per_lot
    }
}
