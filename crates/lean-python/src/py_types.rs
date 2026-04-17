use lean_core::{Market, Resolution, Symbol};
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

#[pymethods]
impl PySecurity {
    #[getter]
    fn symbol(&self) -> PySymbol {
        self.inner.clone()
    }

    fn __repr__(&self) -> String {
        format!("Security('{}')", self.inner.inner.value)
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
