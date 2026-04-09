use lean_core::Symbol;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EtfConstituentData {
    pub symbol: Symbol,
    pub etf_ticker: String,
    pub weight: Decimal,
    pub shares_held: Decimal,
    pub market_value: Decimal,
}
