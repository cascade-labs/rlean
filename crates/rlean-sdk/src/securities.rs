//! SDK-owned security subscription, lookup, and projection APIs.

use chrono::NaiveDate;
use rlean_algorithm::qc_algorithm::{OptionFilter, QcAlgorithm};
use rlean_core::{Market, Price, Symbol, SymbolOptionsExt};
use rlean_orders::{SlippageModel, VolumeShareSlippageModel};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::types::{OptionRight, OptionStyle, Resolution, SecurityType};
#[cfg(feature = "python")]
use pyo3::prelude::*;
#[cfg(feature = "python")]
use pyo3::types::PyAnyMethods;

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "Security"))]
pub struct SecurityHandle {
    symbol: Symbol,
    algorithm: Option<Arc<Mutex<QcAlgorithm>>>,
}

/// LEAN-compatible volume-share price-impact model.
#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "VolumeShareSlippageModel"))]
pub struct VolumeShareSlippageModelHandle {
    inner: Arc<dyn SlippageModel>,
}

impl VolumeShareSlippageModelHandle {
    pub fn new(volume_limit: f64, price_impact: f64) -> Self {
        let volume_limit = Decimal::from_f64_retain(volume_limit).unwrap_or(Decimal::ZERO);
        let price_impact = Decimal::from_f64_retain(price_impact).unwrap_or(Decimal::ZERO);
        Self {
            inner: Arc::new(VolumeShareSlippageModel::new(volume_limit, price_impact)),
        }
    }

    fn model(&self) -> Arc<dyn SlippageModel> {
        self.inner.clone()
    }
}

impl Default for VolumeShareSlippageModelHandle {
    fn default() -> Self {
        Self {
            inner: Arc::new(VolumeShareSlippageModel::default()),
        }
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl VolumeShareSlippageModelHandle {
    #[new]
    #[pyo3(signature = (volume_limit=0.025, price_impact=0.1))]
    fn py_new(volume_limit: f64, price_impact: f64) -> Self {
        Self::new(volume_limit, price_impact)
    }
}

impl SecurityHandle {
    pub fn new(symbol: Symbol) -> Self {
        Self {
            symbol,
            algorithm: None,
        }
    }

    pub fn with_algorithm(symbol: Symbol, algorithm: Arc<Mutex<QcAlgorithm>>) -> Self {
        Self {
            symbol,
            algorithm: Some(algorithm),
        }
    }

    pub fn symbol_inner(&self) -> &Symbol {
        &self.symbol
    }
    pub fn symbol(&self) -> SymbolHandle {
        SymbolHandle::new(self.symbol.clone())
    }

    /// Current market price of the security, or `0.0` when the handle is not
    /// bound to a live algorithm or the security is uninitialized.
    pub fn price(&self) -> f64 {
        self.algorithm
            .as_ref()
            .and_then(|algorithm| read_algorithm_security_price(algorithm, &self.symbol).ok())
            .unwrap_or(0.0)
    }

    /// Exchange-hours projection for the security's market.
    pub fn exchange(&self) -> SecurityExchangeView {
        let hours = rlean_core::MarketHoursDatabase::global().exchange_hours(&self.symbol);
        SecurityExchangeView { hours }
    }

    pub fn set_slippage_model(&self, model: &VolumeShareSlippageModelHandle) -> Result<(), String> {
        let algorithm = self
            .algorithm
            .as_ref()
            .ok_or_else(|| "security handle is not bound to an algorithm".to_string())?;
        let security = algorithm
            .lock()
            .unwrap()
            .securities
            .get(&self.symbol)
            .ok_or_else(|| format!("security {} is not registered", self.symbol))?;
        security.set_slippage_model(model.model());
        Ok(())
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl SecurityHandle {
    #[getter(symbol)]
    fn py_symbol(&self) -> SymbolHandle {
        self.symbol()
    }

    #[getter(price)]
    fn py_price(&self) -> f64 {
        self.price()
    }

    #[getter(exchange)]
    fn py_exchange(&self) -> SecurityExchangeView {
        self.exchange()
    }

    #[pyo3(name = "set_slippage_model")]
    fn py_set_slippage_model(&self, model: &VolumeShareSlippageModelHandle) -> PyResult<()> {
        self.set_slippage_model(model)
            .map_err(pyo3::exceptions::PyValueError::new_err)
    }
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "Option"))]
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
    pub fn symbol(&self) -> SymbolHandle {
        SymbolHandle::new(self.symbol.clone())
    }

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

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl OptionSecurityHandle {
    #[getter(symbol)]
    fn py_symbol(&self) -> SymbolHandle {
        self.symbol()
    }

    #[pyo3(name = "set_filter")]
    fn py_set_filter(
        &self,
        min_strike_rank: i32,
        max_strike_rank: i32,
        min_expiry_days: i32,
        max_expiry_days: i32,
    ) {
        self.set_filter(
            min_strike_rank,
            max_strike_rank,
            min_expiry_days,
            max_expiry_days,
        );
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "SecurityExchangeHours"))]
#[derive(Clone)]
pub struct ExchangeHoursHandle {
    hours: std::sync::Arc<rlean_core::ExchangeHours>,
}

impl ExchangeHoursHandle {
    /// True when the exchange is open at any point in `[start, end)` (or exactly
    /// at `start` when `end` is `None`/equal). Mirrors LEAN `is_open`.
    pub fn is_open(
        &self,
        start: chrono::NaiveDateTime,
        end: Option<chrono::NaiveDateTime>,
    ) -> bool {
        match end {
            None => self.hours.is_open_at_local_naive(start),
            Some(end) if start == end => self.hours.is_open_at_local_naive(start),
            Some(end) => {
                let mut cursor = start;
                while cursor < end {
                    if self.hours.is_open_at_local_naive(cursor) {
                        return true;
                    }
                    cursor += chrono::Duration::minutes(1);
                    if cursor.date() > end.date() {
                        break;
                    }
                }
                false
            }
        }
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl ExchangeHoursHandle {
    #[pyo3(name = "is_open", signature = (start, end=None, extended=None))]
    fn py_is_open(
        &self,
        start: chrono::NaiveDateTime,
        end: Option<chrono::NaiveDateTime>,
        extended: Option<bool>,
    ) -> bool {
        let _ = extended.unwrap_or(false);
        self.is_open(start, end)
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "SecurityExchange"))]
#[derive(Clone)]
pub struct SecurityExchangeView {
    pub hours: std::sync::Arc<rlean_core::ExchangeHours>,
}

impl SecurityExchangeView {
    /// Exchange trading hours for this security.
    pub fn hours(&self) -> ExchangeHoursHandle {
        ExchangeHoursHandle {
            hours: self.hours.clone(),
        }
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl SecurityExchangeView {
    #[getter(hours)]
    fn py_hours(&self) -> ExchangeHoursHandle {
        self.hours()
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "Symbol"))]
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
    pub fn create(
        ticker: String,
        security_type: Option<SecurityType>,
        market: Option<String>,
    ) -> SymbolHandle {
        let market = market.map(Market::new);
        SymbolHandle::new(Symbol::create_with_security_type(
            &ticker,
            security_type.unwrap_or(SecurityType::Equity).into(),
            market,
        ))
    }
    pub fn create_equity(ticker: String, market: Option<String>) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        SymbolHandle::new(Symbol::create_equity(&ticker, &market))
    }
    pub fn create_index(ticker: String, market: Option<String>) -> SymbolHandle {
        let market = market.map(Market::new).unwrap_or_else(Market::usa);
        SymbolHandle::new(Symbol::create_index(&ticker, &market))
    }
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
            right.into(),
            style.unwrap_or(OptionStyle::American).into(),
            &market,
        ))
    }
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
            right.into(),
            style.unwrap_or(OptionStyle::American).into(),
            &market,
        ))
    }
    pub fn value(&self) -> &str {
        &self.inner.value
    }

    pub fn ticker(&self) -> &str {
        &self.inner.permtick
    }

    pub fn sid(&self) -> u64 {
        self.inner.id.sid
    }

    /// Underlying symbol for derivative symbols (options), else `None`.
    /// Mirrors LEAN `Symbol.Underlying`.
    pub fn underlying(&self) -> Option<SymbolHandle> {
        self.inner.underlying().cloned().map(SymbolHandle::new)
    }

    /// Security type of the symbol. Mirrors LEAN `Symbol.SecurityType`.
    pub fn security_type(&self) -> SecurityType {
        self.inner.security_type().into()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl SymbolHandle {
    #[staticmethod]
    #[pyo3(name = "create")]
    #[pyo3(signature = (ticker, security_type=None, market=None))]
    pub fn py_create(
        ticker: String,
        security_type: Option<SecurityType>,
        market: Option<String>,
    ) -> SymbolHandle {
        Self::create(ticker, security_type, market)
    }

    #[staticmethod]
    #[pyo3(name = "create_equity", signature = (ticker, market=None))]
    pub fn py_create_equity(ticker: String, market: Option<String>) -> SymbolHandle {
        Self::create_equity(ticker, market)
    }

    #[staticmethod]
    #[pyo3(name = "create_index", signature = (ticker, market=None))]
    pub fn py_create_index(ticker: String, market: Option<String>) -> SymbolHandle {
        Self::create_index(ticker, market)
    }

    #[staticmethod]
    #[pyo3(name = "create_option_osi", signature = (underlying, strike, expiry, right, style=None, market=None))]
    pub fn py_create_option_osi(
        underlying: SymbolHandle,
        strike: f64,
        expiry: NaiveDate,
        right: OptionRight,
        style: Option<OptionStyle>,
        market: Option<String>,
    ) -> SymbolHandle {
        Self::create_option_osi(underlying, strike, expiry, right, style, market)
    }

    #[getter(value)]
    fn py_value(&self) -> &str {
        self.value()
    }

    #[getter(ticker)]
    fn py_ticker(&self) -> &str {
        self.ticker()
    }

    #[getter(sid)]
    fn py_sid(&self) -> u64 {
        self.sid()
    }

    #[getter(underlying)]
    fn py_underlying(&self) -> Option<SymbolHandle> {
        self.underlying()
    }

    #[getter(security_type)]
    fn py_security_type(&self) -> SecurityType {
        self.security_type()
    }

    fn __str__(&self) -> &str {
        self.value()
    }

    fn __repr__(&self) -> String {
        format!("Symbol({})", self.value())
    }

    fn __hash__(&self) -> isize {
        let hash = self.sid() as isize;
        if hash == -1 {
            -2
        } else {
            hash
        }
    }

    fn __richcmp__(
        &self,
        other: &pyo3::Bound<'_, pyo3::PyAny>,
        op: pyo3::pyclass::CompareOp,
    ) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
        let Ok(other) = other.extract::<pyo3::PyRef<'_, SymbolHandle>>() else {
            return Ok(other.py().NotImplemented());
        };
        let result = match op {
            pyo3::pyclass::CompareOp::Eq => self.sid() == other.sid(),
            pyo3::pyclass::CompareOp::Ne => self.sid() != other.sid(),
            pyo3::pyclass::CompareOp::Lt => self.sid() < other.sid(),
            pyo3::pyclass::CompareOp::Le => self.sid() <= other.sid(),
            pyo3::pyclass::CompareOp::Gt => self.sid() > other.sid(),
            pyo3::pyclass::CompareOp::Ge => self.sid() >= other.sid(),
        };
        Ok(pyo3::types::PyBool::new(other.py(), result)
            .to_owned()
            .into_any()
            .unbind())
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

#[cfg_attr(feature = "python", pyo3::pyclass(name = "SecurityManager"))]
#[derive(Clone)]
pub struct SecurityManagerHandle {
    algorithm: Arc<Mutex<QcAlgorithm>>,
}

impl SecurityManagerHandle {
    pub fn new(algorithm: Arc<Mutex<QcAlgorithm>>) -> Self {
        Self { algorithm }
    }

    /// Security handle for `symbol`, bound to the live algorithm so price and
    /// exchange projections resolve against current state.
    pub fn get(&self, symbol: &Symbol) -> SecurityHandle {
        SecurityHandle::with_algorithm(symbol.clone(), self.algorithm.clone())
    }

    /// True when the algorithm's security manager holds `symbol`.
    pub fn contains_key(&self, symbol: &Symbol) -> bool {
        self.algorithm.lock().unwrap().securities.contains(symbol)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl SecurityManagerHandle {
    #[pyo3(name = "__getitem__")]
    fn py_getitem(&self, symbol: SymbolHandle) -> SecurityHandle {
        self.get(symbol.inner())
    }

    #[pyo3(name = "contains_key")]
    fn py_contains_key(&self, symbol: SymbolHandle) -> bool {
        self.contains_key(symbol.inner())
    }
}

#[derive(Debug, Clone)]
pub struct AlgorithmSettings {
    values: HashMap<String, AlgorithmSettingValue>,
}

impl Default for AlgorithmSettings {
    fn default() -> Self {
        let mut values = HashMap::new();
        values.insert(
            "free_portfolio_value_percentage".to_string(),
            AlgorithmSettingValue::Float(0.0025),
        );
        values.insert(
            "minimum_order_margin_portfolio_percentage".to_string(),
            AlgorithmSettingValue::Float(0.001),
        );
        Self { values }
    }
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

#[cfg_attr(feature = "python", pyo3::pyclass(name = "AlgorithmSettings"))]
#[derive(Clone)]
pub struct AlgorithmSettingsHandle {
    inner: Arc<Mutex<AlgorithmSettings>>,
    algorithm: Option<Arc<Mutex<QcAlgorithm>>>,
}

impl AlgorithmSettingsHandle {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(AlgorithmSettings::default())),
            algorithm: None,
        }
    }

    pub fn from_shared(
        inner: Arc<Mutex<AlgorithmSettings>>,
        algorithm: Arc<Mutex<QcAlgorithm>>,
    ) -> Self {
        Self {
            inner,
            algorithm: Some(algorithm),
        }
    }

    /// Value stored under `name`, defaulting to integer `0` when unset.
    pub fn get(&self, name: &str) -> AlgorithmSettingValue {
        self.inner.lock().unwrap().get(name)
    }

    /// Store `value` under `name`.
    pub fn set(&self, name: impl Into<String>, value: AlgorithmSettingValue) {
        let name = name.into();
        self.inner.lock().unwrap().set(name.clone(), value.clone());

        let Some(algorithm) = &self.algorithm else {
            return;
        };
        let decimal = match value {
            AlgorithmSettingValue::Integer(value) => Some(Decimal::from(value)),
            AlgorithmSettingValue::Float(value) => Decimal::from_f64_retain(value),
            AlgorithmSettingValue::Bool(_) | AlgorithmSettingValue::String(_) => None,
        };
        let Some(decimal) = decimal else {
            return;
        };
        let mut algorithm = algorithm.lock().unwrap();
        match name.as_str() {
            "free_portfolio_value" => algorithm.free_portfolio_value = Some(decimal),
            "free_portfolio_value_percentage" => {
                algorithm.free_portfolio_value_percentage = decimal;
                algorithm.free_portfolio_value = None;
            }
            "minimum_order_margin_portfolio_percentage" => {
                algorithm.minimum_order_margin_portfolio_percentage = decimal;
            }
            _ => {}
        }
    }
}

impl Default for AlgorithmSettingsHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl AlgorithmSettingsHandle {
    #[pyo3(name = "get")]
    fn py_get(&self, name: String) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
        let value = self.get(&name);
        Python::attach(|py| algorithm_setting_value_to_py(py, value))
    }

    #[pyo3(name = "set")]
    fn py_set(&self, name: String, value: pyo3::Bound<'_, pyo3::PyAny>) -> pyo3::PyResult<()> {
        let parsed = algorithm_setting_value_from_py(&value)?;
        self.set(name, parsed);
        Ok(())
    }

    // LEAN exposes settings as named attributes (e.g. `settings.minimum_order_margin_portfolio_percentage`).
    // Back them with the generic key/value store so any LEAN setting name reads/writes
    // without a dedicated field per property.
    fn __getattr__(&self, name: String) -> pyo3::PyResult<pyo3::Py<pyo3::PyAny>> {
        let value = self.get(&name);
        Python::attach(|py| algorithm_setting_value_to_py(py, value))
    }

    fn __setattr__(&self, name: String, value: pyo3::Bound<'_, pyo3::PyAny>) -> pyo3::PyResult<()> {
        let parsed = algorithm_setting_value_from_py(&value)?;
        self.set(name, parsed);
        Ok(())
    }
}

#[cfg(feature = "python")]
fn algorithm_setting_value_to_py(
    py: Python<'_>,
    value: AlgorithmSettingValue,
) -> PyResult<Py<PyAny>> {
    use pyo3::types::{PyBool, PyString};
    Ok(match value {
        AlgorithmSettingValue::Bool(value) => PyBool::new(py, value).to_owned().into_any().unbind(),
        AlgorithmSettingValue::Integer(value) => value.into_pyobject(py)?.into_any().unbind(),
        AlgorithmSettingValue::Float(value) => value.into_pyobject(py)?.into_any().unbind(),
        AlgorithmSettingValue::String(value) => {
            PyString::new(py, &value).to_owned().into_any().unbind()
        }
    })
}

#[cfg(feature = "python")]
fn algorithm_setting_value_from_py(value: &Bound<'_, PyAny>) -> PyResult<AlgorithmSettingValue> {
    if let Ok(value) = value.extract::<bool>() {
        return Ok(AlgorithmSettingValue::Bool(value));
    }
    if let Ok(value) = value.extract::<i64>() {
        return Ok(AlgorithmSettingValue::Integer(value));
    }
    if let Ok(value) = value.extract::<f64>() {
        return Ok(AlgorithmSettingValue::Float(value));
    }
    if let Ok(value) = value.extract::<String>() {
        return Ok(AlgorithmSettingValue::String(value));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "setting value must be bool, int, float, or str",
    ))
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
    // Seed the portfolio holding's last_price too, so a security seeded from
    // history (FuncSecuritySeeder / get_last_known_prices) reports a non-zero
    // market price in snapshots before the live feed delivers its first bar.
    // Skip Base (custom-data) symbols: they are not positions and must not be
    // materialized into the holdings map.
    if symbol.security_type() != rlean_core::SecurityType::Base {
        alg.portfolio.update_prices(symbol, price);
    }
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
    use crate::types::{OptionRight, OptionStyle};
    use chrono::NaiveDate;
    use rlean_core::{Resolution, SecurityType};
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
        assert_eq!(
            settings.get("free_portfolio_value_percentage"),
            AlgorithmSettingValue::Float(0.0025)
        );
        assert_eq!(
            settings.get("minimum_order_margin_portfolio_percentage"),
            AlgorithmSettingValue::Float(0.001)
        );
    }
}
