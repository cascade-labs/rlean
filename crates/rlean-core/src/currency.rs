use serde::{Deserialize, Serialize};

/// A traded currency pair (e.g., EURUSD = EUR/USD).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CurrencyPair {
    pub base: String,
    pub quote: String,
}

impl CurrencyPair {
    pub fn new(base: impl Into<String>, quote: impl Into<String>) -> Self {
        CurrencyPair {
            base: base.into().to_uppercase(),
            quote: quote.into().to_uppercase(),
        }
    }

    pub fn from_ticker(ticker: &str) -> Option<Self> {
        let t = ticker.to_uppercase();
        // Standard 6-char forex pairs: EURUSD
        if t.len() == 6 {
            return Some(CurrencyPair::new(&t[..3], &t[3..]));
        }
        // Slash-delimited: EUR/USD or EUR-USD
        if let Some(idx) = t.find('/').or_else(|| t.find('-')) {
            return Some(CurrencyPair::new(&t[..idx], &t[idx + 1..]));
        }
        None
    }

    pub fn ticker(&self) -> String {
        format!("{}{}", self.base, self.quote)
    }
}

impl std::fmt::Display for CurrencyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.base, self.quote)
    }
}

/// All well-known currency codes.
pub mod codes {
    pub const USD: &str = "USD";
    pub const EUR: &str = "EUR";
    pub const GBP: &str = "GBP";
    pub const JPY: &str = "JPY";
    pub const AUD: &str = "AUD";
    pub const CAD: &str = "CAD";
    pub const CHF: &str = "CHF";
    pub const NZD: &str = "NZD";
    pub const CNH: &str = "CNH";
    pub const HKD: &str = "HKD";
    pub const BTC: &str = "BTC";
    pub const ETH: &str = "ETH";
    pub const USDT: &str = "USDT";
}
