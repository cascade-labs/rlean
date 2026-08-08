use crate::{Market, OptionRight, OptionStyle, Price, SecurityType};
use chrono::NaiveDate;
use rust_decimal::prelude::ToPrimitive;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

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
    const LEAN_MARKET_OFFSET: u64 = 100;
    const LEAN_STRIKE_SCALE_OFFSET: u64 = 100_000;
    const LEAN_STRIKE_OFFSET: u64 = 10_000_000;
    const LEAN_OPTION_STYLE_OFFSET: u64 = 10_000_000_000_000;
    const LEAN_DAYS_OFFSET: u64 = 100_000_000_000_000;
    const LEAN_PUT_CALL_OFFSET: u64 = 10_000_000_000_000_000_000;

    /// Produces the canonical textual C# LEAN equity SID used by persisted
    /// data contracts. `first_ticker` and `first_date` are the first map-file
    /// entry, matching `SecurityIdentifier.GetFirstTickerAndDate`.
    pub fn lean_equity_sid(
        first_ticker: &str,
        market: &Market,
        first_date: NaiveDate,
    ) -> anyhow::Result<String> {
        Self::lean_security_sid(first_ticker, market, SecurityType::Equity, first_date)
    }

    /// Produces a canonical textual C# LEAN SID for a non-derivative security.
    pub fn lean_security_sid(
        first_ticker: &str,
        market: &Market,
        security_type: SecurityType,
        first_date: NaiveDate,
    ) -> anyhow::Result<String> {
        Self::lean_sid(
            first_ticker,
            market,
            security_type,
            first_date,
            Price::ZERO,
            OptionRight::Call,
            OptionStyle::American,
            None,
        )
    }

    /// Produces the canonical textual C# LEAN option SID, including its
    /// canonical underlying SID.
    pub fn lean_option_sid(
        first_ticker: &str,
        market: &Market,
        underlying_security_type: SecurityType,
        underlying_first_date: NaiveDate,
        expiry: NaiveDate,
        strike: Price,
        right: OptionRight,
        style: OptionStyle,
    ) -> anyhow::Result<String> {
        let underlying = Self::lean_sid(
            first_ticker,
            market,
            underlying_security_type,
            underlying_first_date,
            Price::ZERO,
            OptionRight::Call,
            OptionStyle::American,
            None,
        )?;
        let security_type = match underlying_security_type {
            SecurityType::Index => SecurityType::IndexOption,
            SecurityType::Future => SecurityType::FutureOption,
            _ => SecurityType::Option,
        };
        Self::lean_sid(
            first_ticker,
            market,
            security_type,
            expiry,
            strike,
            right,
            style,
            Some(&underlying),
        )
    }

    fn lean_sid(
        ticker: &str,
        market: &Market,
        security_type: SecurityType,
        date: NaiveDate,
        strike: Price,
        right: OptionRight,
        style: OptionStyle,
        underlying: Option<&str>,
    ) -> anyhow::Result<String> {
        let oa_epoch = NaiveDate::from_ymd_opt(1899, 12, 30).expect("valid OLE Automation epoch");
        let oa_days = date.signed_duration_since(oa_epoch).num_days();
        anyhow::ensure!(
            oa_days >= 0,
            "LEAN SID date predates OLE Automation epoch: {date}"
        );
        let (encoded_strike, strike_scale) = Self::lean_normalize_strike(strike)?;
        let properties = u64::from(right as u8) * Self::LEAN_PUT_CALL_OFFSET
            + (oa_days as u64) * Self::LEAN_DAYS_OFFSET
            + u64::from(style as u8) * Self::LEAN_OPTION_STYLE_OFFSET
            + encoded_strike * Self::LEAN_STRIKE_OFFSET
            + strike_scale * Self::LEAN_STRIKE_SCALE_OFFSET
            + u64::from(Market::encode(market.as_str())) * Self::LEAN_MARKET_OFFSET
            + u64::from(security_type as u8);
        let mut text = format!(
            "{} {}",
            ticker.to_ascii_uppercase(),
            Self::lean_encode_base36(properties)
        );
        if let Some(underlying) = underlying {
            text.push('|');
            text.push_str(underlying);
        }
        Ok(text)
    }

    fn lean_normalize_strike(mut strike: Price) -> anyhow::Result<(u64, u64)> {
        if strike.is_zero() {
            return Ok((0, 0));
        }
        strike *= Price::from(1_000_000_u64);
        let mut scale = 0_u64;
        while (strike % Price::TEN).is_zero() {
            strike /= Price::TEN;
            scale += 1;
        }
        let encoded = strike.trunc().to_i64().ok_or_else(|| {
            anyhow::anyhow!("option strike cannot be encoded in a LEAN SID: {strike}")
        })?;
        const NEGATIVE_MASK: u64 = 1 << 19;
        const MAX_STRIKE_PRICE: u64 = 475_711;
        anyhow::ensure!(
            encoded.unsigned_abs() < MAX_STRIKE_PRICE,
            "option strike is outside the LEAN SID range: {strike}"
        );
        let encoded = if encoded < 0 {
            encoded.unsigned_abs() | NEGATIVE_MASK
        } else {
            encoded as u64
        };
        Ok((encoded, scale))
    }

    fn lean_encode_base36(mut value: u64) -> String {
        if value == 0 {
            return "0".to_string();
        }
        let mut output = Vec::with_capacity(15);
        while value != 0 {
            let digit = (value % 36) as u8;
            output.push(if digit < 10 {
                b'0' + digit
            } else {
                b'A' + digit - 10
            });
            value /= 36;
        }
        output.reverse();
        String::from_utf8(output).expect("base36 is ASCII")
    }

    pub fn generate_base(ticker: &str, market: &Market, data_type: &str) -> Self {
        let base_ticker = format!("{}:{}", data_type.to_uppercase(), ticker.to_uppercase());
        let sid = Self::hash_sid(
            &base_ticker,
            market,
            SecurityType::Base,
            None,
            None,
            None,
            None,
        );
        SecurityIdentifier {
            ticker: base_ticker,
            market: market.clone(),
            security_type: SecurityType::Base,
            expiry: None,
            strike: None,
            option_right: None,
            option_style: None,
            sid,
        }
    }

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

    pub fn generate_crypto_future(ticker: &str, market: &Market) -> Self {
        let sid = Self::hash_sid(
            ticker,
            market,
            SecurityType::CryptoFuture,
            None,
            None,
            None,
            None,
        );
        SecurityIdentifier {
            ticker: ticker.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::CryptoFuture,
            expiry: None,
            strike: None,
            option_right: None,
            option_style: None,
            sid,
        }
    }

    pub fn generate_index(ticker: &str, market: &Market) -> Self {
        let sid = Self::hash_sid(ticker, market, SecurityType::Index, None, None, None, None);
        SecurityIdentifier {
            ticker: ticker.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::Index,
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

    pub fn generate_index_option(
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
            SecurityType::IndexOption,
            Some(expiry),
            Some(strike),
            Some(right),
            Some(style),
        );
        SecurityIdentifier {
            ticker: underlying.to_uppercase(),
            market: market.clone(),
            security_type: SecurityType::IndexOption,
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
#[derive(Debug, Clone)]
pub struct Symbol(Arc<SymbolInner>);

#[derive(Debug, Serialize, Deserialize)]
pub struct SymbolInner {
    pub id: Arc<SecurityIdentifier>,
    /// Current market ticker (may differ from id.ticker for mapped symbols).
    pub value: Arc<str>,
    /// Canonical value used for display — usually matches `value`.
    pub permtick: Arc<str>,
    /// For derivatives: the underlying symbol.
    pub underlying: Option<Arc<Symbol>>,
}

impl Deref for Symbol {
    type Target = SymbolInner;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Serialize for Symbol {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Symbol {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Self(Arc::new(SymbolInner::deserialize(deserializer)?)))
    }
}

impl Symbol {
    pub fn from_parts(
        id: SecurityIdentifier,
        value: impl Into<Arc<str>>,
        permtick: impl Into<Arc<str>>,
        underlying: Option<Symbol>,
    ) -> Self {
        Self(Arc::new(SymbolInner {
            id: Arc::new(id),
            value: value.into(),
            permtick: permtick.into(),
            underlying: underlying.map(Arc::new),
        }))
    }

    pub fn default_market_for_security_type(security_type: SecurityType) -> Market {
        match security_type {
            SecurityType::Crypto | SecurityType::CryptoFuture => Market::binance(),
            SecurityType::Forex => Market::forex(),
            _ => Market::usa(),
        }
    }

    pub fn create_with_security_type(
        ticker: &str,
        security_type: SecurityType,
        market: Option<Market>,
    ) -> Self {
        let market =
            market.unwrap_or_else(|| Self::default_market_for_security_type(security_type));
        match security_type {
            SecurityType::Crypto => Symbol::create_crypto(ticker, &market),
            SecurityType::CryptoFuture => Symbol::create_crypto_future(ticker, &market),
            SecurityType::Forex => Symbol::create_forex(ticker),
            SecurityType::Index => Symbol::create_index(ticker, &market),
            _ => Symbol::create_equity(ticker, &market),
        }
    }

    pub fn equity_underlying_for_option(value: &Symbol, market: &Market) -> Symbol {
        if value.security_type() == SecurityType::Equity {
            value.clone()
        } else {
            Symbol::create_equity(value.permtick.as_ref(), market)
        }
    }

    pub fn index_underlying_for_option(value: &Symbol, market: &Market) -> Symbol {
        if value.security_type() == SecurityType::Index {
            value.clone()
        } else {
            Symbol::create_index(value.permtick.as_ref(), market)
        }
    }

    pub fn create_base(data_type: &str, ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_base(ticker, market, data_type);
        let value = ticker.to_uppercase();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
    }

    pub fn create_base_with_underlying(
        data_type: &str,
        underlying: Symbol,
        market: &Market,
    ) -> Self {
        let id = SecurityIdentifier::generate_base(underlying.value.as_ref(), market, data_type);
        let value = underlying.value.to_string();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            Some(underlying),
        )
    }

    pub fn create_equity(ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_equity(ticker, market);
        let value = ticker.to_uppercase();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
    }

    pub fn create_forex(ticker: &str) -> Self {
        let id = SecurityIdentifier::generate_forex(ticker);
        let value = ticker.to_uppercase();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
    }

    pub fn create_crypto(ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_crypto(ticker, market);
        let value = ticker.to_uppercase();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
    }

    pub fn create_crypto_future(ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_crypto_future(ticker, market);
        let value = ticker.to_uppercase();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
    }

    pub fn create_index(ticker: &str, market: &Market) -> Self {
        let id = SecurityIdentifier::generate_index(ticker, market);
        let value = ticker.to_uppercase();
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
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
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            Some(underlying),
        )
    }

    pub fn create_future(ticker: &str, market: &Market, expiry: NaiveDate) -> Self {
        let id = SecurityIdentifier::generate_future(ticker, market, expiry);
        let value = format!("{} {}", ticker.to_uppercase(), expiry.format("%Y%m%d"));
        Symbol::from_parts(
            id,
            Arc::<str>::from(value.as_str()),
            Arc::<str>::from(value),
            None,
        )
    }

    pub fn security_type(&self) -> SecurityType {
        self.id.security_type
    }

    pub fn market(&self) -> &Market {
        &self.id.market
    }

    pub fn sid(&self) -> u64 {
        self.id.sid
    }

    pub fn value(&self) -> &str {
        self.value.as_ref()
    }

    pub fn permtick(&self) -> &str {
        self.permtick.as_ref()
    }

    pub fn underlying(&self) -> Option<&Symbol> {
        self.underlying.as_deref()
    }

    pub fn with_sid(&self, sid: u64) -> Self {
        if self.id.sid == sid {
            return self.clone();
        }
        let mut id = self.id.as_ref().clone();
        id.sid = sid;
        Self(Arc::new(SymbolInner {
            id: Arc::new(id),
            value: self.value.clone(),
            permtick: self.permtick.clone(),
            underlying: self.underlying.clone(),
        }))
    }

    pub fn with_value(&self, value: &str) -> Self {
        Self(Arc::new(SymbolInner {
            id: self.id.clone(),
            value: Arc::from(value),
            permtick: Arc::from(value),
            underlying: self.underlying.clone(),
        }))
    }

    /// Return this security with the ticker mapped for a particular date while
    /// preserving its permanent ticker and SID. This mirrors LEAN's
    /// `Symbol.UpdateMappedSymbol`: FB and META are values of one security, not
    /// two independent identities.
    pub fn with_mapped_value(&self, value: &str) -> Self {
        Self(Arc::new(SymbolInner {
            id: self.id.clone(),
            value: Arc::from(value),
            permtick: self.permtick.clone(),
            underlying: self.underlying.clone(),
        }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Market;

    #[test]
    fn create_with_security_type_uses_lean_default_markets() {
        let crypto = Symbol::create_with_security_type("BTCUSDT", SecurityType::Crypto, None);
        let forex = Symbol::create_with_security_type("EURUSD", SecurityType::Forex, None);
        let equity = Symbol::create_with_security_type("SPY", SecurityType::Equity, None);

        assert_eq!(crypto.market().as_str(), Market::BINANCE);
        assert_eq!(forex.market().as_str(), Market::FOREX);
        assert_eq!(equity.market().as_str(), Market::USA);
    }

    #[test]
    fn option_underlying_projection_preserves_index_symbols() {
        let market = Market::usa();
        let index = Symbol::create_index("SPX", &market);
        let equity = Symbol::create_equity("SPY", &market);

        assert_eq!(
            Symbol::index_underlying_for_option(&index, &market).security_type(),
            SecurityType::Index
        );
        assert_eq!(
            Symbol::index_underlying_for_option(&equity, &market).security_type(),
            SecurityType::Index
        );
        assert_eq!(
            Symbol::equity_underlying_for_option(&index, &market).security_type(),
            SecurityType::Equity
        );
    }

    #[test]
    fn symbol_clone_shares_immutable_storage() {
        let market = Market::usa();
        let underlying = Symbol::create_equity("SPY", &market);
        let option = Symbol::create_option(
            underlying,
            &market,
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            Price::new(411, 0),
            OptionRight::Call,
            OptionStyle::American,
        );
        let clone = option.clone();

        assert!(Arc::ptr_eq(&option.0, &clone.0));
    }

    #[test]
    fn mapped_value_preserves_permanent_identity() {
        let symbol = Symbol::create_equity("META", &Market::usa());
        let mapped = symbol.with_mapped_value("FB");

        assert_eq!(mapped.value.as_ref(), "FB");
        assert_eq!(mapped.permtick.as_ref(), "META");
        assert_eq!(mapped.id.sid, symbol.id.sid);
        assert_eq!(mapped, symbol);
    }

    #[test]
    fn symbol_serde_roundtrip_preserves_public_shape() {
        let market = Market::usa();
        let symbol = Symbol::create_equity("SPY", &market);
        let json = serde_json::to_string(&symbol).unwrap();

        assert!(json.contains("\"value\":\"SPY\""));
        assert!(json.contains("\"permtick\":\"SPY\""));

        let restored: Symbol = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, symbol);
        assert_eq!(restored.value.as_ref(), "SPY");
    }

    #[test]
    fn canonical_option_sids_match_csharp_lean_vectors() {
        let first_date = NaiveDate::from_ymd_opt(1998, 1, 2).unwrap();
        let expiry = NaiveDate::from_ymd_opt(2020, 5, 21).unwrap();
        let cases = [
            (
                OptionStyle::American,
                OptionRight::Call,
                Price::new(100, 0),
                "AAPL XEOLB4YAUINA|AAPL R735QTJ8XC9X",
            ),
            (
                OptionStyle::American,
                OptionRight::Put,
                Price::new(100, 0),
                "AAPL 31DSLGKXI4C12|AAPL R735QTJ8XC9X",
            ),
            (
                OptionStyle::European,
                OptionRight::Call,
                Price::new(100, 0),
                "AAPL XEOOUQW0NLD2|AAPL R735QTJ8XC9X",
            ),
            (
                OptionStyle::European,
                OptionRight::Put,
                Price::new(100, 0),
                "AAPL 31DSP06V7XEQU|AAPL R735QTJ8XC9X",
            ),
            (
                OptionStyle::American,
                OptionRight::Call,
                Price::new(1, 2),
                "AAPL XEOLB4YALY06|AAPL R735QTJ8XC9X",
            ),
            (
                OptionStyle::American,
                OptionRight::Put,
                Price::new(1, 2),
                "AAPL 31DSLGKXHVRDY|AAPL R735QTJ8XC9X",
            ),
        ];

        for (style, right, strike, expected) in cases {
            assert_eq!(
                SecurityIdentifier::lean_option_sid(
                    "AAPL",
                    &Market::usa(),
                    SecurityType::Equity,
                    first_date,
                    expiry,
                    strike,
                    right,
                    style,
                )
                .unwrap(),
                expected
            );
        }
    }
}
