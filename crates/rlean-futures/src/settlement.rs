use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone, Copy)]
pub enum SettlementMethod {
    Physical, // actual delivery of commodity
    Cash,     // mark-to-market cash settlement
}

#[derive(Debug, Clone)]
pub struct SettlementResult {
    pub cash_change: Decimal,       // cash credited/debited
    pub underlying_change: Decimal, // shares/units delivered (0 for cash-settled)
    pub message: String,
}

/// Compute settlement for an expiring futures position.
pub fn settle_futures_position(
    position_quantity: Decimal,
    settlement_price: Decimal,
    average_price: Decimal,
    method: SettlementMethod,
    contract_multiplier: Decimal,
) -> SettlementResult {
    match method {
        SettlementMethod::Cash => {
            let pnl = (settlement_price - average_price) * position_quantity * contract_multiplier;
            SettlementResult {
                cash_change: pnl,
                underlying_change: dec!(0),
                message: format!("Cash settled at {settlement_price}"),
            }
        }
        SettlementMethod::Physical => {
            let underlying = position_quantity * contract_multiplier;
            let cash = -underlying * settlement_price;
            SettlementResult {
                cash_change: cash,
                underlying_change: underlying,
                message: format!("Physical delivery at {settlement_price}"),
            }
        }
    }
}
