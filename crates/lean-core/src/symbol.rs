use crate::{Market, OptionRight, OptionStyle, Price, SecurityType};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::hash::{Hash, Hasher};

/// Immutable, globally unique identifier for any tradeable instrument.
/// Mirrors LEAN's `SecurityIdentifier` + `Symbol` pair.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SecurityIdentifier {
    /// Human-readable ticker at time of creation (may be stale — use Symbol.ticker for current)
    pub ticker: String,
    pub market: Market,
    pub security_type: SecurityType,
    /// For options/futures: expiry date
    pub expiry: Option<NaiveDate>,
    /// For options: strike price (scaled integer stored as Decimal)
    pub strike: Option<Price>,
    pub option_right: Option<OptionRight>,
    pub option_style: Option<OptionStyle>,
    /// Unique 64-bit hash used as lookup key in all internal maps.
    pub sid: u64,
}

impl SecurityIdentifier {
    pub fn generate_equity(ticker: &str, market: &Market) -> Self {
        let sid = Self::hash_sid(ticker, market, SecurityType::Equity, None, None, None, None);
        SecurityIdentifier {
            ticker: ticker.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::Equity,
            expiry: None,
            strike: None,
            option_right: None,
            option_style: None,
            sid,
        }
    }

    pub fn generate_forex(ticker: &str) -> Self {
        let market = Market::forex();
        let sid = Self::hash_sid(ticker, &market, SecurityType::Forex, None, None, None, None);
        SecurityIdentifier {
            ticker: ticker.to_uppercase(),
            market,
            security_type: SecurityType::Forex,
            expiry: None,
            strike: None,
            option_right: None,
            option_style: None,
            sid,
        }
    }

    pub fn generate_crypto(ticker: &str, market: &Market) -> Self {
        let sid = Self::hash_sid(ticker, market, SecurityType::Crypto, None, None, None, None);
        SecurityIdentifier {
            ticker: ticker.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::Crypto,
            expiry: None,
            strike: None,
            option_right: None,
            option_style: None,
            sid,
        }
    }

    pub fn generate_option(
        underlying: &str,
        market: &Market,
        expiry: NaiveDate,
        strike: Price,
        right: OptionRight,
        style: OptionStyle,
    ) -> Self {
        let sid = Self::hash_sid(
            underlying,
            market,
            SecurityType::Option,
            Some(expiry),
            Some(strike),
            Some(right),
            Some(style),
        );
        SecurityIdentifier {
            ticker: underlying.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::Option,
            expiry: Some(expiry),
            strike: Some(strike),
            option_right: Some(right),
            option_style: Some(style),
            sid,
        }
    }

    pub fn generate_future(ticker: &str, market: &Market, expiry: NaiveDate) -> Self {
        let sid = Self::hash_sid(
            ticker,
            market,
            SecurityType::Future,
            Some(expiry),
            None,
            None,
            None,
        );
        SecurityIdentifier {
            ticker: ticker.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::Future,
            expiry: Some(expiry),
            strike: None,
            option_right: None,
            option_style: None,
            sid,
        }
    }

    fn hash_sid(
        ticker: &str,
        market: &Market,
        sec_type: SecurityType,
        expiry: Option<NaiveDate>,
        strike: Option<Price>,
        right: Option<OptionRight>,
        style: Option<OptionStyle>,
    ) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut h = DefaultHasher::new();
        ticker.to_uppercase().hash(&mut h);
        market.as_str().hash(&mut h);
        (sec_type as u8).hash(&mut h);
        if let Some(e) = expiry {
            e.hash(&mut h);
        }
        if let Some(s) = strike {
            s.to_string().hash(&mut h);
        }
        if let Some(r) = right {
            (r as u8).hash(&mut h);
        }
        if let Some(st) = style {
            (st as u8).hash(&mut h);
        }
        std::hash::Hasher::finish(&h)
    }
}

impl fmt::Display for SecurityIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} {}", self.ticker, self.market, self.security_type)
    }
}

/// High-level handle for a tradeable instrument.
/// Cheap to clone — arc'd inner data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub id: SecurityIdentifier,
    /// Current market ticker (may differ from id.ticker for mapped symbols).
    pub value: String,
    /// Canonical value used for display — usually matches `value`.
    pub permtick: String,
    /// For derivatives: the underlying symbol.
    pub underlying: Option<Box<Symbol>>,
}

impl Symbol {
    pub fn create_equity(ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_equity(ticker, market);
        Symbol {
            value: ticker.to_uppercase(),
            permtick: ticker.to_uppercase(),
            id,
            underlying: None,
        }
    }

    pub fn create_forex(ticker: &str) -> Self {
        let id = SecurityIdentifier::generate_forex(ticker);
        Symbol {
            value: ticker.to_uppercase(),
            permtick: ticker.to_uppercase(),
            id,
            underlying: None,
        }
    }

    pub fn create_crypto(ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_crypto(ticker, market);
        Symbol {
            value: ticker.to_uppercase(),
            permtick: ticker.to_uppercase(),
            id,
            underlying: None,
        }
    }

    pub fn create_option(
        underlying: Symbol,
        market: &Market,
        expiry: NaiveDate,
        strike: Price,
        right: OptionRight,
        style: OptionStyle,
    ) -> Self {
        let id = SecurityIdentifier::generate_option(
            &underlying.value,
            market,
            expiry,
            strike,
            right,
            style,
        );
        let value = format!(
            "{} {} {} {} {}",
            underlying.value,
            expiry.format("%Y%m%d"),
            right,
            strike,
            style,
        );
        Symbol {
            value: value.clone(),
            permtick: value,
            id,
            underlying: Some(Box::new(underlying)),
        }
    }

    pub fn create_future(ticker: &str, market: &Market, expiry: NaiveDate) -> Self {
        let id = SecurityIdentifier::generate_future(ticker, market, expiry);
        let value = format!("{} {}", ticker.to_uppercase(), expiry.format("%Y%m%d"));
        Symbol {
            value: value.clone(),
            permtick: value,
            id,
            underlying: None,
        }
    }

    pub fn security_type(&self) -> SecurityType {
        self.id.security_type
    }

    pub fn market(&self) -> &Market {
        &self.id.market
    }

    pub fn has_underlying(&self) -> bool {
        self.underlying.is_some()
    }
}

impl PartialEq for Symbol {
    fn eq(&self, other: &Self) -> bool {
        self.id.sid == other.id.sid
    }
}

impl Eq for Symbol {}

impl Hash for Symbol {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.sid.hash(state);
    }
}

impl fmt::Display for Symbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

/// Static symbol properties (tick size, lot size, pip size, etc.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolProperties {
    pub description: String,
    pub quote_currency: String,
    pub contract_multiplier: f64,
    pub minimum_price_variation: f64,
    pub lot_size: f64,
    pub market_ticker: String,
    pub minimum_order_size: Option<f64>,
    pub price_magnifier: f64,
}

impl Default for SymbolProperties {
    fn default() -> Self {
        SymbolProperties {
            description: String::new(),
            quote_currency: "USD".into(),
            contract_multiplier: 1.0,
            minimum_price_variation: 0.01,
            lot_size: 1.0,
            market_ticker: String::new(),
            minimum_order_size: None,
            price_magnifier: 1.0,
        }
    }
}
