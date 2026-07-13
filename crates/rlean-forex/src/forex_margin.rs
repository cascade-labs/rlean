use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Required margin for a forex position.
/// margin_required = (notional_value) / leverage
pub fn required_margin(quantity: Decimal, price: Decimal, leverage: Decimal) -> Decimal {
    if leverage.is_zero() {
        return dec!(0);
    }
    (quantity.abs() * price) / leverage
}

/// Maximum position size given available margin.
pub fn max_position_size(available_margin: Decimal, price: Decimal, leverage: Decimal) -> Decimal {
    if price.is_zero() {
        return dec!(0);
    }
    available_margin * leverage / price
}
