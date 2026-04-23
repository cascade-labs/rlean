use lean_core::{DataNormalizationMode, Market, Resolution, Symbol};
use pyo3::prelude::*;
use std::collections::HashMap;

/// Python-visible Resolution enum.
/// LEAN uses PascalCase: Resolution.Daily, Resolution.Minute, Resolution.Hour, etc.
#[pyclass(name = "Resolution", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyResolution {
    Tick = 0,
    Second = 1,
    Minute = 2,
    Hour = 3,
    Daily = 4,
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

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<PyObject> {
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
#[derive(Debug, Clone)]
pub struct PySecurity {
    pub inner: PySymbol,
}

impl PySecurity {
    pub fn from_symbol(sym: PySymbol) -> Self {
        Self { inner: sym }
    }
}

#[pymethods]
impl PySecurity {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.inner.clone()
    }

    /// LEAN API: ``security.SetDataNormalizationMode(DataNormalizationMode.Adjusted)``
    /// rlean applies Adjusted normalization by default; this is a no-op for API compatibility.
    fn set_data_normalization_mode(&self, _mode: PyDataNormalizationMode) {}

    /// LEAN API: ``security.SetLeverage(2.0)`` — no-op; rlean does not support leverage multipliers.
    fn set_leverage(&self, _leverage: f64) {}

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<PyObject> {
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
    fn __getattr__(&self, name: &str) -> PyResult<PyObject> {
        Python::with_gil(|py| {
            Ok(0i64.into_pyobject(py).unwrap().into_any().unbind())
        })
        .map_err(|e: PyErr| e)
    }
}

/// Helper: Symbol from ticker string assuming US equity.
pub fn symbol_from_str(ticker: &str) -> Symbol {
    Symbol::create_equity(ticker, &Market::usa())
}

// ─── PyOptionSecurity ─────────────────────────────────────────────────────────

/// LEAN API: returned by `self.add_option("SPY")`.
/// Exposes `.symbol` (the canonical option symbol) and `.set_filter()` (no-op in rlean).
#[pyclass(name = "Option")]
#[derive(Debug, Clone)]
pub struct PyOptionSecurity {
    pub canonical: PySymbol,
}

#[pymethods]
impl PyOptionSecurity {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.canonical.clone()
    }

    /// No-op filter stub — LEAN uses this to limit the option universe.
    /// In rlean the universe is already constrained by the data provider.
    #[pyo3(signature = (*_args, **_kwargs))]
    fn set_filter(
        &self,
        _args: &Bound<'_, pyo3::types::PyTuple>,
        _kwargs: Option<Bound<'_, pyo3::types::PyDict>>,
    ) {
    }

    fn __repr__(&self) -> String {
        format!("Option('{}')", self.canonical.inner.value)
    }
}

// ─── PySecurityEntry ──────────────────────────────────────────────────────────

/// LEAN API: a single security in the securities collection.
/// Returned by `self.securities[symbol]`.
#[pyclass(name = "Security", frozen)]
#[derive(Debug, Clone)]
pub struct PySecurityEntry {
    #[pyo3(get)]
    pub price: f64,
    symbol_inner: PySymbol,
}

#[pymethods]
impl PySecurityEntry {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.symbol_inner.clone()
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<PyObject> {
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

    pub fn build_entry(symbol: Symbol, price: f64) -> PySecurityEntry {
        PySecurityEntry {
            price,
            symbol_inner: PySymbol { inner: symbol },
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

    fn __len__(&self) -> usize {
        self.entries.len()
    }

    fn __repr__(&self) -> String {
        format!("SecurityManager({} securities)", self.entries.len())
    }
}

fn resolve_sid(arg: &Bound<'_, PyAny>) -> PyResult<u64> {
    if let Ok(sym) = arg.downcast::<PySymbol>() {
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
            PyDataNormalizationMode::ForwardPanamaCanal => DataNormalizationMode::ForwardPanamaCanal,
            PyDataNormalizationMode::BackwardPanamaCanal => DataNormalizationMode::BackwardPanamaCanal,
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
        assert_eq!(pascal_to_snake("Ticker"), "ticker", "Symbol.Ticker → ticker");
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
}
