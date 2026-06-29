//! SDK-owned security subscription, lookup, and projection APIs.

use chrono::NaiveDate;
use lean_algorithm::qc_algorithm::{OptionFilter, QcAlgorithm};
use lean_core::{
    Market, OptionRight, OptionStyle, Price, Resolution, SecurityType, Symbol, SymbolOptionsExt,
};
use lean_sdk_annotations::{sdk_bind, sdk_getter, sdk_method, sdk_static};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "Security")]
pub struct SecurityHandle {
    symbol: Symbol,
}

impl SecurityHandle {
    pub fn new(symbol: Symbol) -> Self {
        Self { symbol }
    }

    pub fn symbol_inner(&self) -> &Symbol {
        &self.symbol
    }

    #[sdk_getter]
    pub fn symbol(&self) -> SymbolHandle {
        SymbolHandle::new(self.symbol.clone())
    }
}

#[derive(Clone)]
#[sdk_bind(py_name = "Option")]
pub struct OptionSecurityHandle {
    symbol: Symbol,
    algorithm: Arc<Mutex<QcAlgorithm>>,
}

impl OptionSecurityHandle {
    pub fn new(symbol: Symbol, algorithm: Arc<Mutex<QcAlgorithm>>) -> Self {
        Self { symbol, algorithm }
    }

    pub fn symbol_inner(&self) -> &Symbol {
        &self.symbol
    }

    #[sdk_getter]
    pub fn symbol(&self) -> SymbolHandle {
        SymbolHandle::new(self.symbol.clone())
    }

    #[sdk_method]
    pub fn set_filter(
        &self,
        min_strike_rank: i32,
        max_strike_rank: i32,
        min_expiry_days: i32,
        max_expiry_days: i32,
    ) {
        self.algorithm.lock().unwrap().set_option_filter(
            &self.symbol,
            OptionFilter {
                min_strike_rank,
                max_strike_rank,
                min_expiry_days,
                max_expiry_days,
            },
        );
    }
}

#[sdk_bind(py_name = "SecurityExchange")]
pub struct SecurityExchangeHandle;

#[sdk_bind(py_name = "SecurityExchangeHours")]
pub struct ExchangeHoursHandle;

#[sdk_bind(py_name = "SecurityManager")]
pub struct SecurityManagerHandle;

#[derive(Debug, Clone)]
#[sdk_bind(
    py_name = "Symbol",
    wraps = "lean_core::Symbol",
    wrap_constructor = "new",
    str = "value",
    repr = "value",
    hash = "sid",
    richcmp = "sid"
)]
pub struct SymbolHandle {
    inner: Symbol,
}

impl SymbolHandle {
    pub fn new(inner: Symbol) -> Self {
        Self { inner }
    }

    pub fn into_inner(self) -> Symbol {
        self.inner
    }

    pub fn inner(&self) -> &Symbol {
        &self.inner
    }

    #[sdk_static]
    pub fn create(
        ticker: String,
        security_type: Option<SecurityType>,
        market: Option<String>,
    ) -> SymbolHandle {
        let market = market.map(Market::new);
        SymbolHandle::new(Symbol::create_with_security_type(
            &ticker,
            security_type.unwrap_or(SecurityType::Equity),
            market,
        ))
    }

    #[sdk_static]
    pub fn create_equity(ticker: String, market: Option<String>) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        SymbolHandle::new(Symbol::create_equity(&ticker, &market))
    }

    #[sdk_static]
    pub fn create_index(ticker: String, market: Option<String>) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        SymbolHandle::new(Symbol::create_index(&ticker, &market))
    }

    #[sdk_static]
    pub fn create_option_osi(
        underlying: SymbolHandle,
        strike: f64,
        expiry: NaiveDate,
        right: OptionRight,
        style: Option<OptionStyle>,
        market: Option<String>,
    ) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        let strike = Decimal::from_f64_retain(strike).unwrap_or(Decimal::ZERO);
        SymbolHandle::new(Symbol::create_option_osi(
            Symbol::equity_underlying_for_option(underlying.inner(), &market),
            strike,
            expiry,
            right,
            style.unwrap_or(OptionStyle::American),
            &market,
        ))
    }

    #[sdk_static]
    pub fn create_index_option_osi(
        underlying: SymbolHandle,
        strike: f64,
        expiry: NaiveDate,
        right: OptionRight,
        style: Option<OptionStyle>,
        market: Option<String>,
    ) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        let strike = Decimal::from_f64_retain(strike).unwrap_or(Decimal::ZERO);
        SymbolHandle::new(Symbol::create_index_option_osi(
            Symbol::index_underlying_for_option(underlying.inner(), &market),
            strike,
            expiry,
            right,
            style.unwrap_or(OptionStyle::American),
            &market,
        ))
    }

    #[sdk_getter]
    pub fn value(&self) -> &str {
        &self.inner.value
    }

    #[sdk_getter]
    pub fn ticker(&self) -> &str {
        &self.inner.permtick
    }

    #[sdk_method]
    pub fn sid(&self) -> u64 {
        self.inner.id.sid
    }
}

#[derive(Debug, Clone)]
pub struct SecuritySubscription {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub security_type: SecurityType,
    pub market: Market,
}

#[derive(Debug, Clone, Default)]
pub struct SecurityLookup {
    by_ticker: HashMap<String, Symbol>,
}

impl SecurityLookup {
    pub fn insert(&mut self, ticker: &str, symbol: Symbol) {
        self.by_ticker.insert(ticker.to_uppercase(), symbol);
    }

    pub fn resolve(&self, ticker: &str) -> Option<Symbol> {
        self.by_ticker.get(&ticker.to_uppercase()).cloned()
    }
}

#[derive(Debug, Clone, Default)]
#[sdk_bind(py_name = "AlgorithmSettings")]
pub struct AlgorithmSettings {
    values: HashMap<String, AlgorithmSettingValue>,
}

impl AlgorithmSettings {
    pub fn set(&mut self, name: impl Into<String>, value: AlgorithmSettingValue) {
        self.values.insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> AlgorithmSettingValue {
        self.values
            .get(name)
            .cloned()
            .unwrap_or(AlgorithmSettingValue::Integer(0))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlgorithmSettingValue {
    Bool(bool),
    Integer(i64),
    Float(f64),
    String(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityProjectionError {
    UninitializedSecurity { symbol_value: String },
    PriceNotFloatRepresentable { symbol_value: String },
}

pub fn read_algorithm_security_price(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    symbol: &Symbol,
) -> Result<f64, SecurityProjectionError> {
    let alg = algorithm.lock().unwrap();
    let Some(security) = alg.securities.get(symbol) else {
        return Err(SecurityProjectionError::UninitializedSecurity {
            symbol_value: symbol.value.to_string(),
        });
    };
    security.current_price().to_f64().ok_or_else(|| {
        SecurityProjectionError::PriceNotFloatRepresentable {
            symbol_value: symbol.value.to_string(),
        }
    })
}

pub fn read_algorithm_security_leverage(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    symbol: &Symbol,
) -> Result<f64, SecurityProjectionError> {
    let alg = algorithm.lock().unwrap();
    let Some(security) = alg.securities.get(symbol) else {
        return Err(SecurityProjectionError::UninitializedSecurity {
            symbol_value: symbol.value.to_string(),
        });
    };
    Ok(security.leverage())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityPriceError {
    InvalidMarketPrice { symbol_value: String },
}

pub fn price_from_float(price: f64) -> Option<Price> {
    if price <= 0.0 || !price.is_finite() {
        return None;
    }
    Decimal::from_f64_retain(price)
}

pub fn set_algorithm_security_price(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    symbol: &Symbol,
    price: Price,
) -> bool {
    let alg = algorithm.lock().unwrap();
    alg.securities.update_price(symbol, price);
    true
}

pub fn set_algorithm_security_price_from_float(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    symbol: &Symbol,
    price: f64,
) -> Result<bool, SecurityPriceError> {
    let price = price_from_float(price).ok_or_else(|| SecurityPriceError::InvalidMarketPrice {
        symbol_value: symbol.value.to_string(),
    })?;
    Ok(set_algorithm_security_price(algorithm, symbol, price))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use lean_core::{OptionRight, OptionStyle, Resolution, SecurityType};
    use rust_decimal_macros::dec;

    #[test]
    fn symbol_handle_constructors_match_lean_defaults() {
        let equity = SymbolHandle::create_equity("spy".to_string(), None);
        assert_eq!(equity.value(), "SPY");
        assert_eq!(equity.ticker(), "SPY");
        assert_eq!(equity.inner().security_type(), SecurityType::Equity);

        let index = SymbolHandle::create_index("spx".to_string(), None);
        assert_eq!(index.value(), "SPX");
        assert_eq!(index.inner().security_type(), SecurityType::Index);

        let option = SymbolHandle::create_option_osi(
            equity,
            411.0,
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            OptionRight::Call,
            None,
            None,
        );
        assert_eq!(option.inner().security_type(), SecurityType::Option);
        assert!(option.value().contains("SPY"));

        let index_option = SymbolHandle::create_index_option_osi(
            index,
            5000.0,
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            OptionRight::Put,
            Some(OptionStyle::European),
            None,
        );
        assert_eq!(
            index_option.inner().security_type(),
            SecurityType::IndexOption
        );
        assert!(index_option.value().contains("SPX"));
    }

    #[test]
    fn security_lookup_resolves_tickers_case_insensitively() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut lookup = SecurityLookup::default();
        lookup.insert("spy", symbol.clone());

        assert_eq!(lookup.resolve("SPY"), Some(symbol.clone()));
        assert_eq!(lookup.resolve("sPy"), Some(symbol));
        assert_eq!(lookup.resolve("QQQ"), None);
    }

    #[test]
    fn security_projection_reads_live_algorithm_price() {
        let algorithm = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let symbol = algorithm
            .lock()
            .unwrap()
            .add_equity("JOBY", Resolution::Minute);

        assert_eq!(
            read_algorithm_security_price(&algorithm, &symbol).unwrap(),
            0.0
        );

        algorithm
            .lock()
            .unwrap()
            .securities
            .update_price(&symbol, dec!(12.5));

        assert_eq!(
            read_algorithm_security_price(&algorithm, &symbol).unwrap(),
            12.5
        );
    }

    #[test]
    fn set_algorithm_security_price_rejects_non_positive_float() {
        let algorithm = Arc::new(Mutex::new(QcAlgorithm::new("test", dec!(100000))));
        let symbol = algorithm
            .lock()
            .unwrap()
            .add_equity("JOBY", Resolution::Minute);

        assert!(set_algorithm_security_price_from_float(&algorithm, &symbol, 0.0).is_err());
        assert!(set_algorithm_security_price_from_float(&algorithm, &symbol, f64::NAN).is_err());
        assert_eq!(
            algorithm
                .lock()
                .unwrap()
                .securities
                .get(&symbol)
                .unwrap()
                .current_price(),
            dec!(0)
        );
    }

    #[test]
    fn algorithm_settings_store_values_and_default_missing_to_zero() {
        let mut settings = AlgorithmSettings::default();
        settings.set("foo", AlgorithmSettingValue::String("bar".to_string()));

        assert_eq!(
            settings.get("foo"),
            AlgorithmSettingValue::String("bar".to_string())
        );
        assert_eq!(settings.get("missing"), AlgorithmSettingValue::Integer(0));
    }
}
