use lean_core::OptionRight;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Intrinsic value: max(0, S-K) for calls, max(0, K-S) for puts.
pub fn intrinsic_value(underlying_price: Decimal, strike: Decimal, right: OptionRight) -> Decimal {
    match right {
        OptionRight::Call => (underlying_price - strike).max(dec!(0)),
        OptionRight::Put  => (strike - underlying_price).max(dec!(0)),
    }
}

/// Option payoff at expiry (same formula).
pub fn payoff(underlying_price: Decimal, strike: Decimal, right: OptionRight) -> Decimal {
    intrinsic_value(underlying_price, strike, right)
}

/// True if the option would be auto-exercised (intrinsic >= $0.01).
pub fn is_auto_exercised(underlying_price: Decimal, strike: Decimal, right: OptionRight) -> bool {
    intrinsic_value(underlying_price, strike, right) >= dec!(0.01)
}

/// Number of underlying shares that change hands on exercise.
///
/// C# LEAN formula:
///   sign = Call → -1, Put → +1
///   quantity = sign * exercise_order_quantity * contract_unit_of_trade (100)
///
/// Positive result = buy underlying, negative = sell underlying.
pub fn get_exercise_quantity(
    exercise_order_quantity: Decimal,
    right: OptionRight,
    contract_unit_of_trade: i64,
) -> Decimal {
    let sign = match right {
        OptionRight::Call => dec!(-1),
        OptionRight::Put  => dec!(1),
    };
    sign * exercise_order_quantity * Decimal::from(contract_unit_of_trade)
}
