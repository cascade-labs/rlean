use chrono::{Datelike, NaiveDate, Timelike};
use lean_algorithm::qc_algorithm::{OptionFilter, QcAlgorithm};
use lean_core::{
    DataNormalizationMode, Market, OptionRight, OptionStyle, Resolution, SecurityType, Symbol,
    SymbolOptionsExt,
};
use pyo3::prelude::*;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Python-visible Resolution enum.
/// QuantConnect Python uses SCREAMING_SNAKE_CASE; PascalCase remains accepted.
#[pyclass(name = "Resolution", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyResolution {
    Tick = 0,
    Second = 1,
    Minute = 2,
    Hour = 3,
    Daily = 4,
}

#[pymethods]
impl PyResolution {
    #[classattr]
    const TICK: Self = Self::Tick;

    #[classattr]
    const SECOND: Self = Self::Second;

    #[classattr]
    const MINUTE: Self = Self::Minute;

    #[classattr]
    const HOUR: Self = Self::Hour;

    #[classattr]
    const DAILY: Self = Self::Daily;
}

impl From<PyResolution> for Resolution {
    fn from(r: PyResolution) -> Self {
        match r {
            PyResolution::Tick => Resolution::Tick,
            PyResolution::Second => Resolution::Second,
            PyResolution::Minute => Resolution::Minute,
            PyResolution::Hour => Resolution::Hour,
            PyResolution::Daily => Resolution::Daily,
        }
    }
}

/// Python-visible Symbol wrapper.
#[pyclass(name = "Symbol", frozen)]
#[derive(Debug, Clone)]
pub struct PySymbol {
    pub inner: Symbol,
}

#[pymethods]
impl PySymbol {
    #[staticmethod]
    #[pyo3(signature = (ticker, security_type=None, market=None))]
    fn create(
        ticker: &str,
        security_type: Option<&Bound<'_, PyAny>>,
        market: Option<&Bound<'_, PyAny>>,
    ) -> Self {
        let security_type = py_security_type(security_type).unwrap_or(SecurityType::Equity);
        let market = py_market(market).unwrap_or_else(|| match security_type {
            SecurityType::Crypto | SecurityType::CryptoFuture => Market::binance(),
            SecurityType::Forex => Market::forex(),
            _ => Market::usa(),
        });
        let inner = match security_type {
            SecurityType::Crypto => Symbol::create_crypto(ticker, &market),
            SecurityType::CryptoFuture => Symbol::create_crypto_future(ticker, &market),
            SecurityType::Forex => Symbol::create_forex(ticker),
            SecurityType::Index => Symbol::create_index(ticker, &market),
            _ => Symbol::create_equity(ticker, &market),
        };
        PySymbol { inner }
    }

    #[staticmethod]
    #[pyo3(name = "Create", signature = (ticker, security_type=None, market=None))]
    fn create_pascal(
        ticker: &str,
        security_type: Option<&Bound<'_, PyAny>>,
        market: Option<&Bound<'_, PyAny>>,
    ) -> Self {
        Self::create(ticker, security_type, market)
    }

    #[staticmethod]
    #[pyo3(signature = (underlying, strike, expiry, right, style=None, market=None))]
    fn create_option_osi(
        underlying: &Bound<'_, PyAny>,
        strike: f64,
        expiry: &Bound<'_, PyAny>,
        right: &Bound<'_, PyAny>,
        style: Option<&Bound<'_, PyAny>>,
        market: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let market = py_market(market).unwrap_or_else(Market::usa);
        let underlying = equity_symbol_from_py(underlying, &market)?;
        let strike = Decimal::from_f64(strike).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("strike must be a finite number")
        })?;
        Ok(PySymbol {
            inner: Symbol::create_option_osi(
                underlying,
                strike,
                py_expiry(expiry)?,
                py_option_right(right)?,
                py_option_style(style)?,
                &market,
            ),
        })
    }

    #[staticmethod]
    #[pyo3(name = "CreateOptionOsi", signature = (underlying, strike, expiry, right, style=None, market=None))]
    fn create_option_osi_pascal(
        underlying: &Bound<'_, PyAny>,
        strike: f64,
        expiry: &Bound<'_, PyAny>,
        right: &Bound<'_, PyAny>,
        style: Option<&Bound<'_, PyAny>>,
        market: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Self::create_option_osi(underlying, strike, expiry, right, style, market)
    }

    #[staticmethod]
    #[pyo3(signature = (underlying, strike, expiry, right, style=None, market=None))]
    fn create_index_option_osi(
        underlying: &Bound<'_, PyAny>,
        strike: f64,
        expiry: &Bound<'_, PyAny>,
        right: &Bound<'_, PyAny>,
        style: Option<&Bound<'_, PyAny>>,
        market: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        let market = py_market(market).unwrap_or_else(Market::usa);
        let underlying = index_symbol_from_py(underlying, &market)?;
        let strike = Decimal::from_f64(strike).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err("strike must be a finite number")
        })?;
        Ok(PySymbol {
            inner: Symbol::create_index_option_osi(
                underlying,
                strike,
                py_expiry(expiry)?,
                py_option_right(right)?,
                py_option_style(style)?,
                &market,
            ),
        })
    }

    #[staticmethod]
    #[pyo3(name = "CreateIndexOptionOsi", signature = (underlying, strike, expiry, right, style=None, market=None))]
    fn create_index_option_osi_pascal(
        underlying: &Bound<'_, PyAny>,
        strike: f64,
        expiry: &Bound<'_, PyAny>,
        right: &Bound<'_, PyAny>,
        style: Option<&Bound<'_, PyAny>>,
        market: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        Self::create_index_option_osi(underlying, strike, expiry, right, style, market)
    }

    #[getter]
    fn value(&self) -> &str {
        &self.inner.value
    }

    #[getter]
    fn ticker(&self) -> &str {
        &self.inner.permtick
    }

    fn __str__(&self) -> &str {
        &self.inner.value
    }
    fn __repr__(&self) -> String {
        format!("Symbol('{}')", self.inner.value)
    }

    fn __hash__(&self) -> u64 {
        self.inner.id.sid
    }

    fn __eq__(&self, other: &PySymbol) -> bool {
        self.inner.id.sid == other.inner.id.sid
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Symbol' object has no attribute '{name}'"
        )))
    }
}

fn py_security_type(value: Option<&Bound<'_, PyAny>>) -> Option<SecurityType> {
    let value = value?;
    if value.is_none() {
        return None;
    }
    if let Ok(py_type) = value.extract::<crate::PySecurityType>() {
        return Some(match py_type {
            crate::PySecurityType::Base => SecurityType::Base,
            crate::PySecurityType::Equity => SecurityType::Equity,
            crate::PySecurityType::Option => SecurityType::Option,
            crate::PySecurityType::Forex => SecurityType::Forex,
            crate::PySecurityType::Future => SecurityType::Future,
            crate::PySecurityType::Cfd => SecurityType::Cfd,
            crate::PySecurityType::Crypto => SecurityType::Crypto,
            crate::PySecurityType::Index => SecurityType::Index,
            crate::PySecurityType::IndexOption => SecurityType::IndexOption,
            crate::PySecurityType::CryptoFuture => SecurityType::CryptoFuture,
        });
    }
    if let Ok(raw) = value.extract::<i32>() {
        return match raw {
            0 => Some(SecurityType::Base),
            1 => Some(SecurityType::Equity),
            2 => Some(SecurityType::Option),
            3 => Some(SecurityType::Forex),
            4 => Some(SecurityType::Future),
            5 => Some(SecurityType::Cfd),
            7 => Some(SecurityType::Crypto),
            8 => Some(SecurityType::Index),
            9 => Some(SecurityType::IndexOption),
            11 => Some(SecurityType::CryptoFuture),
            _ => None,
        };
    }
    None
}

fn py_market(value: Option<&Bound<'_, PyAny>>) -> Option<Market> {
    let value = value?;
    if value.is_none() {
        return None;
    }
    value.extract::<String>().ok().map(Market::new)
}

fn equity_symbol_from_py(value: &Bound<'_, PyAny>, market: &Market) -> PyResult<Symbol> {
    if let Ok(symbol) = value.cast::<PySymbol>() {
        return Ok(symbol.get().inner.clone());
    }
    if let Ok(ticker) = value.extract::<String>() {
        return Ok(Symbol::create_equity(&ticker, market));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "underlying must be a Symbol or ticker string",
    ))
}

fn index_symbol_from_py(value: &Bound<'_, PyAny>, market: &Market) -> PyResult<Symbol> {
    if let Ok(symbol) = value.cast::<PySymbol>() {
        let symbol = symbol.get().inner.clone();
        return Ok(match symbol.security_type() {
            SecurityType::Index => symbol,
            _ => Symbol::create_index(&symbol.permtick, market),
        });
    }
    if let Ok(ticker) = value.extract::<String>() {
        return Ok(Symbol::create_index(&ticker, market));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "underlying must be a Symbol or ticker string",
    ))
}

fn py_expiry(value: &Bound<'_, PyAny>) -> PyResult<NaiveDate> {
    if let Ok(date) = value.extract::<NaiveDate>() {
        return Ok(date);
    }
    if let Ok(raw) = value.extract::<i32>() {
        let year = raw / 10000;
        let month = ((raw / 100) % 100) as u32;
        let day = (raw % 100) as u32;
        return NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid expiry date {raw}"))
        });
    }
    if let Ok(raw) = value.extract::<String>() {
        for format in ["%Y-%m-%d", "%Y%m%d", "%y%m%d"] {
            if let Ok(date) = NaiveDate::parse_from_str(raw.trim(), format) {
                return Ok(date);
            }
        }
        return Err(pyo3::exceptions::PyValueError::new_err(format!(
            "invalid expiry date '{raw}'"
        )));
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "expiry must be a date, YYYY-MM-DD string, or YYYYMMDD integer",
    ))
}

fn py_option_right(value: &Bound<'_, PyAny>) -> PyResult<OptionRight> {
    if let Ok(right) = value.extract::<crate::py_options::PyOptionRight>() {
        return Ok(right.inner);
    }
    if let Ok(raw) = value.extract::<i32>() {
        return match raw {
            0 => Ok(OptionRight::Call),
            1 => Ok(OptionRight::Put),
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unsupported option right {raw}"
            ))),
        };
    }
    if let Ok(raw) = value.extract::<String>() {
        return match raw.trim().to_ascii_lowercase().as_str() {
            "c" | "call" => Ok(OptionRight::Call),
            "p" | "put" => Ok(OptionRight::Put),
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unsupported option right '{raw}'"
            ))),
        };
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "right must be OptionRight, 'call'/'put', 'C'/'P', or 0/1",
    ))
}

fn py_option_style(value: Option<&Bound<'_, PyAny>>) -> PyResult<OptionStyle> {
    let Some(value) = value else {
        return Ok(OptionStyle::American);
    };
    if value.is_none() {
        return Ok(OptionStyle::American);
    }
    if let Ok(raw) = value.extract::<i32>() {
        return match raw {
            0 => Ok(OptionStyle::American),
            1 => Ok(OptionStyle::European),
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unsupported option style {raw}"
            ))),
        };
    }
    if let Ok(raw) = value.extract::<String>() {
        return match raw.trim().to_ascii_lowercase().as_str() {
            "american" | "a" => Ok(OptionStyle::American),
            "european" | "e" => Ok(OptionStyle::European),
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unsupported option style '{raw}'"
            ))),
        };
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "style must be 'American', 'European', 0, 1, or None",
    ))
}

impl From<Symbol> for PySymbol {
    fn from(s: Symbol) -> Self {
        PySymbol { inner: s }
    }
}

/// Result of a single indicator update.
#[pyclass(name = "IndicatorResult", frozen, get_all)]
#[derive(Debug, Clone)]
pub struct PyIndicatorResult {
    pub is_ready: bool,
    pub value: f64,
}

#[pymethods]
impl PyIndicatorResult {
    fn __repr__(&self) -> String {
        if self.is_ready {
            format!("IndicatorResult(value={:.6})", self.value)
        } else {
            "IndicatorResult(not_ready)".to_string()
        }
    }
}

/// LEAN Security stub — wraps a Symbol and exposes `.symbol`.
/// Returned by `add_equity`, `add_forex`, `add_crypto` to match LEAN's API
/// where those methods return a Security, not a Symbol directly.
#[pyclass(name = "Security", frozen)]
#[derive(Clone)]
pub struct PySecurity {
    pub inner: PySymbol,
    pub algorithm: Option<Arc<Mutex<QcAlgorithm>>>,
}

impl PySecurity {
    pub fn from_symbol(sym: PySymbol) -> Self {
        Self {
            inner: sym,
            algorithm: None,
        }
    }

    pub fn from_algorithm_symbol(sym: PySymbol, algorithm: Arc<Mutex<QcAlgorithm>>) -> Self {
        Self {
            inner: sym,
            algorithm: Some(algorithm),
        }
    }
}

#[pymethods]
impl PySecurity {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.inner.clone()
    }

    #[getter]
    fn exchange(&self) -> PySecurityExchange {
        PySecurityExchange {
            hours: PyExchangeHours,
        }
    }

    #[getter(Exchange)]
    fn exchange_pascal(&self) -> PySecurityExchange {
        self.exchange()
    }

    /// LEAN API: ``security.SetDataNormalizationMode(DataNormalizationMode.Adjusted)``
    /// rlean applies Adjusted normalization by default; this is a no-op for API compatibility.
    fn set_data_normalization_mode(&self, _mode: PyDataNormalizationMode) {}

    #[getter]
    fn leverage(&self) -> PyResult<f64> {
        let Some(algorithm) = &self.algorithm else {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "Security leverage is only available for initialized algorithm securities",
            ));
        };
        let alg = algorithm.lock().unwrap();
        let Some(security) = alg.securities.get(&self.inner.inner) else {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                "Security {} is not initialized",
                self.inner.inner.value
            )));
        };
        Ok(security.leverage())
    }

    /// LEAN API: ``security.SetLeverage(2.0)``.
    fn set_leverage(&self, leverage: f64) -> PyResult<()> {
        let Some(algorithm) = &self.algorithm else {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(
                "Security.SetLeverage requires an initialized algorithm security",
            ));
        };
        algorithm
            .lock()
            .unwrap()
            .register_security_leverage(&self.inner.inner, leverage);
        Ok(())
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Security' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!("Security('{}')", self.inner.inner.value)
    }
}

// ─── PyAlgorithmSettings ──────────────────────────────────────────────────────

/// LEAN API: `self.Settings` — algorithm settings bag.
/// rlean does not act on these settings; they are accepted for API compatibility.
#[pyclass(name = "AlgorithmSettings")]
#[derive(Debug, Clone, Default)]
pub struct PyAlgorithmSettings {}

#[pymethods]
impl PyAlgorithmSettings {
    #[new]
    pub fn new() -> Self {
        PyAlgorithmSettings {}
    }

    /// Accept any attribute set without error.
    fn __setattr__(&mut self, _name: &str, _value: &Bound<'_, PyAny>) {}

    /// Accept any attribute get; return 0 as default.
    fn __getattr__(&self, _name: &str) -> PyResult<Py<PyAny>> {
        Python::attach(|py| Ok(0i64.into_pyobject(py).unwrap().into_any().unbind()))
            .map_err(|e: PyErr| e)
    }
}

/// Helper: Symbol from ticker string assuming US equity.
pub fn symbol_from_str(ticker: &str) -> Symbol {
    Symbol::create_equity(ticker, &Market::usa())
}

// ─── PyOptionSecurity ─────────────────────────────────────────────────────────

/// LEAN API: returned by `self.add_option("SPY")`.
/// Exposes `.symbol` (the canonical option symbol) and `.set_filter()`.
#[pyclass(name = "Option")]
#[derive(Clone)]
pub struct PyOptionSecurity {
    pub canonical: PySymbol,
    pub algorithm: Arc<Mutex<QcAlgorithm>>,
}

#[pymethods]
impl PyOptionSecurity {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.canonical.clone()
    }

    /// LEAN API: option.set_filter(min_strike_rank, max_strike_rank, min_expiry, max_expiry)
    #[pyo3(signature = (min_strike_rank, max_strike_rank, min_expiry_days=0, max_expiry_days=35))]
    fn set_filter(
        &self,
        min_strike_rank: i32,
        max_strike_rank: i32,
        min_expiry_days: i32,
        max_expiry_days: i32,
    ) {
        self.algorithm.lock().unwrap().set_option_filter(
            &self.canonical.inner,
            OptionFilter {
                min_strike_rank,
                max_strike_rank,
                min_expiry_days,
                max_expiry_days,
            },
        );
    }

    fn __repr__(&self) -> String {
        format!("Option('{}')", self.canonical.inner.value)
    }
}

// ─── PySecurityEntry ──────────────────────────────────────────────────────────

/// LEAN API: a single security in the securities collection.
/// Returned by `self.securities[symbol]`.
#[pyclass(name = "Security", frozen)]
#[derive(Clone)]
pub struct PySecurityEntry {
    #[pyo3(get)]
    pub price: f64,
    symbol_inner: PySymbol,
    algorithm: Arc<Mutex<QcAlgorithm>>,
}

#[pymethods]
impl PySecurityEntry {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.symbol_inner.clone()
    }

    #[getter]
    fn exchange(&self) -> PySecurityExchange {
        PySecurityExchange {
            hours: PyExchangeHours,
        }
    }

    #[getter(Exchange)]
    fn exchange_pascal(&self) -> PySecurityExchange {
        self.exchange()
    }

    #[getter]
    fn leverage(&self) -> PyResult<f64> {
        let alg = self.algorithm.lock().unwrap();
        let Some(security) = alg.securities.get(&self.symbol_inner.inner) else {
            return Err(pyo3::exceptions::PyRuntimeError::new_err(format!(
                "Security {} is not initialized",
                self.symbol_inner.inner.value
            )));
        };
        Ok(security.leverage())
    }

    fn set_leverage(&self, leverage: f64) {
        self.algorithm
            .lock()
            .unwrap()
            .register_security_leverage(&self.symbol_inner.inner, leverage);
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Security' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "Security('{}', price={:.2})",
            self.symbol_inner.inner.value, self.price
        )
    }
}

#[pyclass(name = "SecurityExchange", frozen)]
#[derive(Debug, Clone)]
pub struct PySecurityExchange {
    #[pyo3(get)]
    hours: PyExchangeHours,
}

#[pymethods]
impl PySecurityExchange {
    #[getter(Hours)]
    fn hours_pascal(&self) -> PyExchangeHours {
        self.hours.clone()
    }
}

#[pyclass(name = "SecurityExchangeHours", frozen)]
#[derive(Debug, Clone)]
pub struct PyExchangeHours;

#[pymethods]
impl PyExchangeHours {
    #[pyo3(signature = (start, _end=None, _extended_market_hours=false))]
    fn is_open(
        &self,
        start: &Bound<'_, PyAny>,
        _end: Option<&Bound<'_, PyAny>>,
        _extended_market_hours: bool,
    ) -> PyResult<bool> {
        let dt = start.extract::<chrono::NaiveDateTime>()?;
        let weekday = dt.weekday().number_from_monday();
        if weekday > 5 {
            return Ok(false);
        }
        let minutes = dt.hour() * 60 + dt.minute();
        Ok((9 * 60 + 30..16 * 60).contains(&minutes))
    }

    #[pyo3(name = "IsOpen", signature = (start, end=None, extended_market_hours=false))]
    fn is_open_pascal(
        &self,
        start: &Bound<'_, PyAny>,
        end: Option<&Bound<'_, PyAny>>,
        extended_market_hours: bool,
    ) -> PyResult<bool> {
        self.is_open(start, end, extended_market_hours)
    }
}

// ─── PySecurityManager ────────────────────────────────────────────────────────

/// LEAN API: `self.securities` — collection of all subscribed securities.
/// Supports `self.securities[symbol]` to get a Security by symbol.
#[pyclass(name = "SecurityManager", frozen)]
pub struct PySecurityManager {
    entries: HashMap<u64, PySecurityEntry>,
}

impl PySecurityManager {
    pub fn from_entries(entries: HashMap<u64, PySecurityEntry>) -> Self {
        PySecurityManager { entries }
    }

    pub fn build_entry(
        symbol: Symbol,
        price: f64,
        algorithm: Arc<Mutex<QcAlgorithm>>,
    ) -> PySecurityEntry {
        PySecurityEntry {
            price,
            symbol_inner: PySymbol { inner: symbol },
            algorithm,
        }
    }
}

#[pymethods]
impl PySecurityManager {
    fn __getitem__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<PySecurityEntry> {
        let sid = resolve_sid(symbol)?;
        self.entries
            .get(&sid)
            .cloned()
            .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err("Security not found"))
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> bool {
        resolve_sid(symbol)
            .map(|sid| self.entries.contains_key(&sid))
            .unwrap_or(false)
    }

    fn contains_key(&self, symbol: &Bound<'_, PyAny>) -> bool {
        self.__contains__(symbol)
    }

    #[pyo3(name = "ContainsKey")]
    fn contains_key_pascal(&self, symbol: &Bound<'_, PyAny>) -> bool {
        self.__contains__(symbol)
    }

    fn __len__(&self) -> usize {
        self.entries.len()
    }

    fn __repr__(&self) -> String {
        format!("SecurityManager({} securities)", self.entries.len())
    }
}

fn resolve_sid(arg: &Bound<'_, PyAny>) -> PyResult<u64> {
    if let Ok(sym) = arg.cast::<PySymbol>() {
        return Ok(sym.get().inner.id.sid);
    }
    if let Ok(ticker) = arg.extract::<String>() {
        // Fallback: create a US equity symbol to get its SID
        let sym = Symbol::create_equity(&ticker, &Market::usa());
        return Ok(sym.id.sid);
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "Expected Symbol or str",
    ))
}

// ─── DataNormalizationMode ────────────────────────────────────────────────────

/// LEAN DataNormalizationMode — controls how historical prices are adjusted.
#[pyclass(name = "DataNormalizationMode", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyDataNormalizationMode {
    Raw = 0,
    Adjusted = 1,
    SplitAdjusted = 2,
    TotalReturn = 3,
    ForwardPanamaCanal = 4,
    BackwardPanamaCanal = 5,
}

impl From<PyDataNormalizationMode> for DataNormalizationMode {
    fn from(m: PyDataNormalizationMode) -> Self {
        match m {
            PyDataNormalizationMode::Raw => DataNormalizationMode::Raw,
            PyDataNormalizationMode::Adjusted => DataNormalizationMode::Adjusted,
            PyDataNormalizationMode::SplitAdjusted => DataNormalizationMode::SplitAdjusted,
            PyDataNormalizationMode::TotalReturn => DataNormalizationMode::TotalReturn,
            PyDataNormalizationMode::ForwardPanamaCanal => {
                DataNormalizationMode::ForwardPanamaCanal
            }
            PyDataNormalizationMode::BackwardPanamaCanal => {
                DataNormalizationMode::BackwardPanamaCanal
            }
        }
    }
}

// ─── MovingAverageType ────────────────────────────────────────────────────────

/// LEAN MovingAverageType — selects which moving average calculation is used by
/// an indicator (e.g., ExponentialMovingAverage vs SimpleMovingAverage smoothing).
#[pyclass(name = "MovingAverageType", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyMovingAverageType {
    Simple = 0,
    Exponential = 1,
    Weighted = 2,
    DoubleExponential = 3,
    TripleExponential = 4,
    Triangular = 5,
    Kama = 6,
    Adaptive = 7,
    LinearWeightedMovingAverage = 8,
    Alma = 9,
    T3 = 10,
    Vwap = 11,
    Hull = 12,
    MidPoint = 13,
    MidPrice = 14,
    Dema = 15,
    Tema = 16,
    Hma = 17,
    Wilders = 18,
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py_qc_algorithm::pascal_to_snake;
    use lean_core::{Market, Symbol};

    fn make_spy_symbol() -> PySymbol {
        PySymbol {
            inner: Symbol::create_equity("SPY", &Market::usa()),
        }
    }

    /// C# LEAN: symbol.Value (PascalCase) must map to snake_case via __getattr__.
    #[test]
    fn symbol_value_getter_returns_value_string() {
        let sym = make_spy_symbol();
        assert_eq!(sym.value(), "SPY");
        assert_eq!(sym.ticker(), "SPY");
        assert_eq!(sym.__str__(), "SPY");
    }

    /// pascal_to_snake("Value") == "value" — required for __getattr__ forwarding.
    #[test]
    fn symbol_pascal_names_convert_correctly() {
        assert_eq!(pascal_to_snake("Value"), "value", "Symbol.Value → value");
        assert_eq!(
            pascal_to_snake("Ticker"),
            "ticker",
            "Symbol.Ticker → ticker"
        );
        assert_eq!(pascal_to_snake("HasUnderlying"), "has_underlying");
        assert_eq!(pascal_to_snake("SecurityType"), "security_type");
    }

    /// `symbol == symbol` comparison is by SID.
    #[test]
    fn symbol_eq_by_sid() {
        let a = make_spy_symbol();
        let b = make_spy_symbol();
        assert_eq!(a.inner.id.sid, b.inner.id.sid);
        assert!(a.__eq__(&b));
    }

    /// Symbol hash is stable and based on SID.
    #[test]
    fn symbol_hash_is_sid() {
        let sym = make_spy_symbol();
        assert_eq!(sym.__hash__(), sym.inner.id.sid);
    }

    #[test]
    fn symbol_create_option_osi_builds_option_symbol() {
        Python::initialize();
        Python::attach(|py| {
            let underlying = Py::new(py, make_spy_symbol()).unwrap();
            let expiry = "2025-01-17".into_pyobject(py).unwrap().into_any();
            let right = "call".into_pyobject(py).unwrap().into_any();
            let symbol = PySymbol::create_option_osi(
                underlying.bind(py).as_any(),
                450.0,
                &expiry,
                &right,
                None,
                None,
            )
            .unwrap();
            assert_eq!(symbol.value(), "SPY250117C00450000");
            assert_eq!(symbol.inner.security_type(), SecurityType::Option);
        });
    }

    #[test]
    fn symbol_create_index_option_osi_builds_index_option_symbol() {
        Python::initialize();
        Python::attach(|py| {
            let underlying = "SPX".into_pyobject(py).unwrap().into_any();
            let expiry = 20250117i32.into_pyobject(py).unwrap().into_any();
            let right = "put".into_pyobject(py).unwrap().into_any();
            let style = "European".into_pyobject(py).unwrap().into_any();
            let symbol = PySymbol::create_index_option_osi(
                &underlying,
                4500.0,
                &expiry,
                &right,
                Some(&style),
                None,
            )
            .unwrap();
            assert_eq!(symbol.value(), "SPX250117P04500000");
            assert_eq!(symbol.inner.security_type(), SecurityType::IndexOption);
        });
    }
}
