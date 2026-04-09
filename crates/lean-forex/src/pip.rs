use lean_core::CurrencyPair;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Returns the pip size for a currency pair.
/// Most pairs: 0.0001 (4 decimal places)
/// JPY pairs: 0.01 (2 decimal places)
pub fn pip_size(pair: &CurrencyPair) -> Decimal {
    let quote = pair.quote.to_uppercase();
    if quote.contains("JPY") { dec!(0.01) } else { dec!(0.0001) }
}

/// Convert pips to price units
pub fn pips_to_price(pips: Decimal, pair: &CurrencyPair) -> Decimal {
    pips * pip_size(pair)
}

/// Convert price to pips
pub fn price_to_pips(price_diff: Decimal, pair: &CurrencyPair) -> Decimal {
    if pip_size(pair).is_zero() { return dec!(0); }
    price_diff / pip_size(pair)
}

/// Calculate pip value in account currency (USD).
/// pip_value = (pip_size / quote_price) * lot_size
/// For USD as quote (e.g. EUR/USD): pip_value = pip_size * lot_size
/// For USD as base (e.g. USD/JPY): pip_value = pip_size / current_price * lot_size
pub fn pip_value_usd(
    pair: &CurrencyPair,
    current_price: Decimal,
    lot_size: Decimal, // typically 100_000 for standard lot
) -> Decimal {
    let ps = pip_size(pair);
    let quote = pair.quote.to_uppercase();
    if quote == "USD" {
        ps * lot_size
    } else if pair.base.to_uppercase() == "USD" {
        if current_price.is_zero() { return dec!(0); }
        ps / current_price * lot_size
    } else {
        // cross pair — approximate
        ps * lot_size
    }
}

/// Standard lot sizes
pub const STANDARD_LOT: i64 = 100_000;
pub const MINI_LOT: i64 = 10_000;
pub const MICRO_LOT: i64 = 1_000;
pub const NANO_LOT: i64 = 100;
