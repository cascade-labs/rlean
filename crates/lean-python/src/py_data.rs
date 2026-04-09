use std::collections::HashMap;
use pyo3::prelude::*;
use lean_data::{Slice, TradeBar};
use rust_decimal::prelude::ToPrimitive;
use crate::py_types::{PySymbol, PySecurity};

fn ns_to_iso(ns: i64) -> String {
    use chrono::{DateTime as ChronoDateTime, Utc};
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub)
        .unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Python-visible TradeBar.
#[pyclass(name = "TradeBar", frozen, get_all)]
#[derive(Debug, Clone)]
pub struct PyTradeBar {
    pub open:   f64,
    pub high:   f64,
    pub low:    f64,
    pub close:  f64,
    pub volume: f64,
    pub symbol: PySymbol,
    /// Bar open time as an ISO string. Matches LEAN's `TradeBar.Time`.
    pub time: String,
    /// Bar close time as an ISO string. Matches LEAN's `TradeBar.EndTime`.
    pub end_time: String,
}

impl From<&TradeBar> for PyTradeBar {
    fn from(b: &TradeBar) -> Self {
        PyTradeBar {
            open:     b.open.to_f64().unwrap_or(0.0),
            high:     b.high.to_f64().unwrap_or(0.0),
            low:      b.low.to_f64().unwrap_or(0.0),
            close:    b.close.to_f64().unwrap_or(0.0),
            volume:   b.volume.to_f64().unwrap_or(0.0),
            symbol:   PySymbol { inner: b.symbol.clone() },
            time:     ns_to_iso(b.time.0),
            end_time: ns_to_iso(b.end_time.0),
        }
    }
}

#[pymethods]
impl PyTradeBar {
    fn __repr__(&self) -> String {
        format!(
            "TradeBar({} O={:.2} H={:.2} L={:.2} C={:.2} V={:.0})",
            self.symbol.inner.value,
            self.open, self.high, self.low, self.close, self.volume
        )
    }
}

/// LEAN API: `data.bars` returns a dict-like object that supports
/// both `bars[symbol]` indexing and `.get(symbol)` lookup.
/// Mirrors LEAN's `TradeBars` collection.
#[pyclass(name = "TradeBars")]
#[derive(Debug, Clone)]
pub struct PyTradeBars {
    /// bars keyed by sid
    bars: HashMap<u64, PyTradeBar>,
    /// ticker → sid for string-based lookups
    ticker_to_sid: HashMap<String, u64>,
}

impl PyTradeBars {
    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.downcast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.downcast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return Ok(self.ticker_to_sid.get(&ticker).copied());
        }
        Err(pyo3::exceptions::PyTypeError::new_err("Expected Security, Symbol, or str"))
    }
}

#[pymethods]
impl PyTradeBars {
    /// LEAN API: `data.bars[symbol]` — returns the bar or None.
    fn __getitem__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<Option<PyTradeBar>> {
        Ok(self.resolve_sid(symbol)?.and_then(|sid| self.bars.get(&sid).cloned()))
    }

    /// LEAN API: `data.bars.get(symbol)` — returns None if not present.
    fn get(&self, symbol: &Bound<'_, PyAny>) -> PyResult<Option<PyTradeBar>> {
        Ok(self.resolve_sid(symbol)?.and_then(|sid| self.bars.get(&sid).cloned()))
    }

    fn __len__(&self) -> usize { self.bars.len() }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self.resolve_sid(symbol)?.map(|sid| self.bars.contains_key(&sid)).unwrap_or(false))
    }

    fn values(&self) -> Vec<PyTradeBar> {
        self.bars.values().cloned().collect()
    }

    fn __repr__(&self) -> String {
        format!("TradeBars(count={})", self.bars.len())
    }
}

/// Python-visible Slice — the object delivered to `on_data`.
#[pyclass(name = "Slice")]
#[derive(Debug, Clone)]
pub struct PySlice {
    /// bars keyed by sid
    bars: HashMap<u64, PyTradeBar>,
    /// ticker → sid for string-based lookups
    ticker_to_sid: HashMap<String, u64>,
    #[pyo3(get)]
    pub has_data: bool,
}

impl PySlice {
    pub fn from_slice(slice: &Slice) -> Self {
        let mut bars = HashMap::new();
        let mut ticker_to_sid = HashMap::new();
        for (&sid, bar) in &slice.bars {
            let py_bar = PyTradeBar::from(bar);
            ticker_to_sid.insert(bar.symbol.value.clone(), sid);
            // also insert by base ticker (without market suffix if any)
            ticker_to_sid.insert(bar.symbol.permtick.clone(), sid);
            bars.insert(sid, py_bar);
        }
        PySlice { bars, ticker_to_sid, has_data: slice.has_data }
    }
}

#[pymethods]
impl PySlice {
    /// Get a bar by Symbol object or ticker string.
    /// Returns None if the symbol is not in this slice.
    fn get_bar(&self, symbol: &Bound<'_, PyAny>) -> PyResult<Option<PyTradeBar>> {
        let sid = self.resolve_sid(symbol)?;
        Ok(self.bars.get(&sid).cloned())
    }

    /// LEAN API: `data.get(symbol)` — alias for `get_bar`.
    fn get(&self, symbol: &Bound<'_, PyAny>) -> PyResult<Option<PyTradeBar>> {
        self.get_bar(symbol)
    }

    /// LEAN API: `data[symbol]` — returns None if not present (mirrors LEAN behaviour).
    fn __getitem__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<Option<PyTradeBar>> {
        match self.resolve_sid(symbol) {
            Ok(sid) => Ok(self.bars.get(&sid).cloned()),
            Err(_) => Ok(None),
        }
    }

    /// LEAN API: `data.bars` — returns a TradeBars dict-like object.
    /// Strategies access bars as: `data.bars[symbol]` or `data.bars.get(symbol)`.
    #[getter]
    fn bars(&self) -> PyTradeBars {
        PyTradeBars {
            bars: self.bars.clone(),
            ticker_to_sid: self.ticker_to_sid.clone(),
        }
    }

    /// Return all tickers present in this slice.
    fn tickers(&self) -> Vec<String> {
        self.ticker_to_sid.keys().cloned().collect()
    }

    fn __repr__(&self) -> String {
        format!("Slice(bars={}, has_data={})", self.bars.len(), self.has_data)
    }
}

impl PySlice {
    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<u64> {
        if let Ok(sym) = arg.downcast::<PySymbol>() {
            return Ok(sym.get().inner.id.sid);
        }
        // Accept Security objects directly (mirrors LEAN's data[security] API)
        if let Ok(sec) = arg.downcast::<PySecurity>() {
            return Ok(sec.get().inner.inner.id.sid);
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return self.ticker_to_sid.get(&ticker)
                .copied()
                .ok_or_else(|| pyo3::exceptions::PyKeyError::new_err(
                    format!("Symbol '{}' not in slice", ticker)
                ));
        }
        Err(pyo3::exceptions::PyTypeError::new_err("Expected Security, Symbol, or str"))
    }
}
