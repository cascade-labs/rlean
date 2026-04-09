use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;

/// Overnight swap rate for a currency pair (annualized).
/// Long swap = interest earned on base currency - interest paid on quote currency
/// Short swap = opposite
#[derive(Debug, Clone)]
pub struct SwapRate {
    pub long_rate: Decimal,   // daily swap for long position (can be negative)
    pub short_rate: Decimal,  // daily swap for short position (can be negative)
}

impl SwapRate {
    pub fn new(long_rate: Decimal, short_rate: Decimal) -> Self {
        Self { long_rate, short_rate }
    }
}

/// Default approximate swap rates for major pairs.
/// In a real system these would be fetched from the broker.
pub fn default_swap_rates() -> HashMap<String, SwapRate> {
    let mut rates = HashMap::new();
    // EUR/USD: ECB rate vs Fed rate differential
    rates.insert("EURUSD".to_string(), SwapRate::new(dec!(-0.00002), dec!(-0.00001)));
    rates.insert("GBPUSD".to_string(), SwapRate::new(dec!(-0.00001), dec!(-0.00002)));
    rates.insert("USDJPY".to_string(), SwapRate::new(dec!(0.00003), dec!(-0.00005)));
    rates.insert("USDCHF".to_string(), SwapRate::new(dec!(0.00001), dec!(-0.00003)));
    rates.insert("AUDUSD".to_string(), SwapRate::new(dec!(0.00001), dec!(-0.00003)));
    rates.insert("USDCAD".to_string(), SwapRate::new(dec!(-0.00001), dec!(-0.00001)));
    rates
}

/// Calculate swap charge for holding a position overnight.
/// quantity: position size in units
/// days: number of overnight periods held
pub fn calculate_swap(
    pair_key: &str,
    quantity: Decimal,
    days: Decimal,
    rates: &HashMap<String, SwapRate>,
) -> Decimal {
    let rate = match rates.get(pair_key) {
        Some(r) => r,
        None => return dec!(0),
    };
    let daily_rate = if quantity > dec!(0) { rate.long_rate } else { rate.short_rate };
    quantity.abs() * daily_rate * days
}
