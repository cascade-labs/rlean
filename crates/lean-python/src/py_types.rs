use pyo3::prelude::*;
use lean_core::{Market, Resolution, Symbol};

/// Python-visible Resolution enum.
/// LEAN uses PascalCase: Resolution.Daily, Resolution.Minute, Resolution.Hour, etc.
#[pyclass(name = "Resolution", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyResolution {
    Tick    = 0,
    Second  = 1,
    Minute  = 2,
    Hour    = 3,
    Daily   = 4,
}

impl From<PyResolution> for Resolution {
    fn from(r: PyResolution) -> Self {
        match r {
            PyResolution::Tick   => Resolution::Tick,
            PyResolution::Second => Resolution::Second,
            PyResolution::Minute => Resolution::Minute,
            PyResolution::Hour   => Resolution::Hour,
            PyResolution::Daily  => Resolution::Daily,
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
    fn value(&self) -> &str { &self.inner.value }

    #[getter]
    fn ticker(&self) -> &str { &self.inner.permtick }

    fn __str__(&self) -> &str { &self.inner.value }
    fn __repr__(&self) -> String { format!("Symbol('{}')", self.inner.value) }

    fn __hash__(&self) -> u64 { self.inner.id.sid }

    fn __eq__(&self, other: &PySymbol) -> bool {
        self.inner.id.sid == other.inner.id.sid
    }
}

impl From<Symbol> for PySymbol {
    fn from(s: Symbol) -> Self { PySymbol { inner: s } }
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
