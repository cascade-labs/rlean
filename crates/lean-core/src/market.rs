use once_cell::sync::Lazy;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Market(String);

impl Market {
    pub fn new(name: impl Into<String>) -> Self {
        Market(name.into().to_lowercase())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Market {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

macro_rules! market_const {
    ($name:ident, $val:expr) => {
        pub const $name: &'static str = $val;
    };
}

/// All canonical LEAN markets.
impl Market {
    market_const!(USA, "usa");
    market_const!(FOREX, "forex");
    market_const!(FXCM, "fxcm");
    market_const!(OANDA, "oanda");
    market_const!(DUKASCOPY, "dukascopy");
    market_const!(EVEREST, "everest");
    market_const!(BITFINEX, "bitfinex");
    market_const!(BINANCE, "binance");
    market_const!(BINANCE_US, "binanceus");
    market_const!(BYBIT, "bybit");
    market_const!(COINBASE, "coinbase");
    market_const!(KRAKEN, "kraken");
    market_const!(FTXUS, "ftxus");
    market_const!(BITMEX, "bitmex");
    market_const!(CME, "cme");
    market_const!(NYMEX, "nymex");
    market_const!(CBOT, "cbot");
    market_const!(ICE, "ice");
    market_const!(CFE, "cfe");
    market_const!(CBOE, "cboe");
    market_const!(NYSE, "nyse");
    market_const!(NASDAQ, "nasdaq");
    market_const!(BATS, "bats");
    market_const!(ARCA, "arca");
    market_const!(EDGX, "edgx");
    market_const!(NSE, "nse");
    market_const!(BSE, "bse");
    market_const!(INDIA, "india");
    market_const!(SGX, "sgx");
    market_const!(HKFE, "hkfe");
    market_const!(OSE, "ose");
    market_const!(EUREX, "eurex");
    market_const!(EURONEXT, "euronext");
    market_const!(LSE, "lse");
    market_const!(ASX, "asx");
    market_const!(TSX, "tsx");
    market_const!(TSE, "tse");
    market_const!(UNKNOWN, "unknown");

    pub fn usa() -> Self {
        Market::new(Self::USA)
    }
    pub fn forex() -> Self {
        Market::new(Self::FOREX)
    }
    pub fn binance() -> Self {
        Market::new(Self::BINANCE)
    }
    pub fn coinbase() -> Self {
        Market::new(Self::COINBASE)
    }
    pub fn cme() -> Self {
        Market::new(Self::CME)
    }
    pub fn cboe() -> Self {
        Market::new(Self::CBOE)
    }
}

static MARKET_CODES: Lazy<RwLock<HashMap<String, u32>>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert(Market::USA.to_string(), 1);
    m.insert(Market::FOREX.to_string(), 2);
    m.insert(Market::CME.to_string(), 3);
    m.insert(Market::NYMEX.to_string(), 4);
    m.insert(Market::CBOT.to_string(), 5);
    m.insert(Market::ICE.to_string(), 6);
    m.insert(Market::CFE.to_string(), 7);
    m.insert(Market::CBOE.to_string(), 8);
    m.insert(Market::BINANCE.to_string(), 9);
    m.insert(Market::COINBASE.to_string(), 10);
    m.insert(Market::KRAKEN.to_string(), 11);
    RwLock::new(m)
});

impl Market {
    pub fn encode(market: &str) -> u32 {
        MARKET_CODES.read().get(market).copied().unwrap_or(0)
    }

    pub fn decode(code: u32) -> Option<String> {
        MARKET_CODES
            .read()
            .iter()
            .find(|(_, &v)| v == code)
            .map(|(k, _)| k.clone())
    }

    pub fn register(name: impl Into<String>, code: u32) {
        MARKET_CODES.write().insert(name.into(), code);
    }
}
