use std::collections::HashMap;
use std::sync::Arc;
use pyo3::prelude::*;
use lean_data::{CustomDataPoint, Slice, SubscriptionDataConfig, TradeBar};
use lean_options::OptionChain;
use lean_data::QuoteBar;
use rust_decimal::prelude::ToPrimitive;
use crate::py_types::{PySymbol, PySecurity};
use crate::py_options::{PyOptionChain, PyOptionChains};

fn ns_to_naive(ns: i64) -> chrono::NaiveDateTime {
    use chrono::{DateTime as ChronoDateTime, Utc, TimeZone as _};
    use chrono_tz::US::Eastern;
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub)
        .unwrap_or_default();
    // Deliver bar times in Eastern Time (exchange local), matching LEAN's behavior.
    dt.with_timezone(&Eastern).naive_local()
}

/// Python-visible TradeBar.
///
/// Not `frozen` — Rust mutates fields in-place via `SliceProxy::update` each bar,
/// eliminating all per-day allocation.  Python only gets read-only `#[pyo3(get)]`
/// accessors, so strategies cannot accidentally overwrite bar data.
#[pyclass(name = "TradeBar")]
#[derive(Debug, Clone)]
pub struct PyTradeBar {
    #[pyo3(get)] pub open:     f64,
    #[pyo3(get)] pub high:     f64,
    #[pyo3(get)] pub low:      f64,
    #[pyo3(get)] pub close:    f64,
    #[pyo3(get)] pub volume:   f64,
    #[pyo3(get)] pub symbol:   PySymbol,
    /// Bar open time as a datetime. Matches LEAN's `TradeBar.Time`.
    #[pyo3(get)] pub time:     chrono::NaiveDateTime,
    /// Bar close time as a datetime. Matches LEAN's `TradeBar.EndTime`.
    #[pyo3(get)] pub end_time: chrono::NaiveDateTime,
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
            time:     ns_to_naive(b.time.0),
            end_time: ns_to_naive(b.end_time.0),
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

/// LEAN API: `data.bars` — dict-like bars collection delivered inside Slice.
///
/// Stores `Py<PyTradeBar>` references rather than owned values, so `get()` and
/// `__getitem__` return a Python reference to the pre-allocated bar object with
/// only a refcount bump — zero copies, zero allocation on the hot path.
#[pyclass(name = "TradeBars")]
pub struct PyTradeBars {
    bars: HashMap<u64, Py<PyTradeBar>>,
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
    /// Returns a Python reference to the bar — no data copied.
    fn __getitem__(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        Ok(self.resolve_sid(symbol)?.and_then(|sid| self.bars.get(&sid).map(|b| b.clone_ref(py))))
    }

    /// LEAN API: `data.bars.get(symbol)` — returns None if not present.
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        Ok(self.resolve_sid(symbol)?.and_then(|sid| self.bars.get(&sid).map(|b| b.clone_ref(py))))
    }

    fn __len__(&self) -> usize { self.bars.len() }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self.resolve_sid(symbol)?.map(|sid| self.bars.contains_key(&sid)).unwrap_or(false))
    }

    fn values(&self, py: Python<'_>) -> Vec<Py<PyTradeBar>> {
        self.bars.values().map(|b| b.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        format!("TradeBars(count={})", self.bars.len())
    }
}

/// LEAN API: `QuoteBar.bid` / `QuoteBar.ask` — nested Bar with OHLC.
/// Matches LEAN's C# `Bar` class exposed via Python.
#[pyclass(name = "Bar")]
#[derive(Debug, Clone)]
pub struct PyBar {
    #[pyo3(get)] pub open:  f64,
    #[pyo3(get)] pub high:  f64,
    #[pyo3(get)] pub low:   f64,
    #[pyo3(get)] pub close: f64,
}

#[pymethods]
impl PyBar {
    fn __repr__(&self) -> String {
        format!("Bar(O={:.4} H={:.4} L={:.4} C={:.4})", self.open, self.high, self.low, self.close)
    }
}

/// Python-visible QuoteBar (bid/ask OHLC).
/// LEAN API: `data.quote_bars[symbol]` → QuoteBar
#[pyclass(name = "QuoteBar")]
#[derive(Debug, Clone)]
pub struct PyQuoteBar {
    #[pyo3(get)] pub bid_open:   f64,
    #[pyo3(get)] pub bid_high:   f64,
    #[pyo3(get)] pub bid_low:    f64,
    #[pyo3(get)] pub bid_close:  f64,
    #[pyo3(get)] pub ask_open:   f64,
    #[pyo3(get)] pub ask_high:   f64,
    #[pyo3(get)] pub ask_low:    f64,
    #[pyo3(get)] pub ask_close:  f64,
    #[pyo3(get)] pub bid_size:   f64,
    #[pyo3(get)] pub ask_size:   f64,
    #[pyo3(get)] pub symbol:     PySymbol,
    #[pyo3(get)] pub time:       chrono::NaiveDateTime,
    #[pyo3(get)] pub end_time:   chrono::NaiveDateTime,
}

#[pymethods]
impl PyQuoteBar {
    /// LEAN API: bar.close → mid-close price
    #[getter]
    fn close(&self) -> f64 { (self.bid_close + self.ask_close) / 2.0 }
    /// LEAN API: bar.open → mid-open price
    #[getter]
    fn open(&self) -> f64 { (self.bid_open + self.ask_open) / 2.0 }
    /// LEAN API: qb.bid → Bar(open, high, low, close) for bid side
    #[getter]
    fn bid(&self, py: Python<'_>) -> PyResult<Py<PyBar>> {
        Py::new(py, PyBar { open: self.bid_open, high: self.bid_high, low: self.bid_low, close: self.bid_close })
    }
    /// LEAN API: qb.ask → Bar(open, high, low, close) for ask side
    #[getter]
    fn ask(&self, py: Python<'_>) -> PyResult<Py<PyBar>> {
        Py::new(py, PyBar { open: self.ask_open, high: self.ask_high, low: self.ask_low, close: self.ask_close })
    }
    fn __repr__(&self) -> String {
        format!("QuoteBar({} bid={:.4} ask={:.4})", self.symbol.inner.value, self.bid_close, self.ask_close)
    }
}

impl From<&QuoteBar> for PyQuoteBar {
    fn from(q: &QuoteBar) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        let to_f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);
        let bid_open  = q.bid.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
        let bid_high  = q.bid.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
        let bid_low   = q.bid.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
        let bid_close = q.bid.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
        let ask_open  = q.ask.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
        let ask_high  = q.ask.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
        let ask_low   = q.ask.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
        let ask_close = q.ask.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
        PyQuoteBar {
            bid_open, bid_high, bid_low, bid_close,
            ask_open, ask_high, ask_low, ask_close,
            bid_size: to_f(q.last_bid_size),
            ask_size: to_f(q.last_ask_size),
            symbol:   PySymbol { inner: q.symbol.clone() },
            time:     ns_to_naive(q.time.0),
            end_time: ns_to_naive(q.end_time.0),
        }
    }
}

/// LEAN API: `data.quote_bars` — dict-like quote bars collection.
#[pyclass(name = "QuoteBars")]
pub struct PyQuoteBars {
    bars: HashMap<u64, Py<PyQuoteBar>>,
    ticker_to_sid: HashMap<String, u64>,
}

impl PyQuoteBars {
    pub fn empty() -> Self {
        PyQuoteBars { bars: HashMap::new(), ticker_to_sid: HashMap::new() }
    }

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
impl PyQuoteBars {
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyQuoteBar>>> {
        Ok(self.resolve_sid(symbol)?.and_then(|sid| self.bars.get(&sid).map(|b| b.clone_ref(py))))
    }
    fn __getitem__(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyQuoteBar>>> {
        self.get(py, symbol)
    }
    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self.resolve_sid(symbol)?.map(|sid| self.bars.contains_key(&sid)).unwrap_or(false))
    }
    fn __len__(&self) -> usize { self.bars.len() }
    fn values(&self, py: Python<'_>) -> Vec<Py<PyQuoteBar>> {
        self.bars.values().map(|b| b.clone_ref(py)).collect()
    }
    fn __repr__(&self) -> String { format!("QuoteBars(count={})", self.bars.len()) }
}

/// Python-visible Slice — the object delivered to `on_data`.
///
/// Holds a `Py<PyTradeBars>` reference rather than owning bar data, so the
/// `bars` getter is a single refcount bump (O(1)) instead of a full HashMap clone.
#[pyclass(name = "Slice")]
pub struct PySlice {
    bars_obj: Py<PyTradeBars>,
    quote_bars_obj: Py<PyQuoteBars>,
    option_chains_obj: Py<PyOptionChains>,
    custom_data_obj: Py<PyCustomData>,
    #[pyo3(get)]
    pub has_data: bool,
}

impl PySlice {
    /// Build a self-contained Slice from a Rust Slice.
    /// Used for warmup and tests where no SliceProxy is available.
    pub fn from_slice(py: Python<'_>, slice: &Slice) -> PyResult<Self> {
        let mut bars: HashMap<u64, Py<PyTradeBar>> = HashMap::new();
        let mut ticker_to_sid: HashMap<String, u64> = HashMap::new();
        for (&sid, bar) in &slice.bars {
            let py_bar = Py::new(py, PyTradeBar::from(bar))?;
            ticker_to_sid.insert(bar.symbol.value.clone(), sid);
            ticker_to_sid.insert(bar.symbol.permtick.clone(), sid);
            bars.insert(sid, py_bar);
        }
        let py_bars = Py::new(py, PyTradeBars { bars, ticker_to_sid })?;
        let py_chains = Py::new(py, PyOptionChains::empty())?;
        let py_quote_bars = Py::new(py, PyQuoteBars::empty())?;
        let py_custom = Py::new(py, PyCustomData::empty())?;
        Ok(PySlice { bars_obj: py_bars, quote_bars_obj: py_quote_bars, option_chains_obj: py_chains, custom_data_obj: py_custom, has_data: slice.has_data })
    }
}

#[pymethods]
impl PySlice {
    /// LEAN API: `data.bars` — returns the TradeBars collection (refcount bump only).
    #[getter]
    fn bars(&self, py: Python<'_>) -> Py<PyTradeBars> {
        self.bars_obj.clone_ref(py)
    }

    /// LEAN API: `data.quote_bars` — returns the QuoteBars collection (refcount bump only).
    #[getter]
    fn quote_bars(&self, py: Python<'_>) -> Py<PyQuoteBars> {
        self.quote_bars_obj.clone_ref(py)
    }

    /// LEAN API: `data.option_chains` — returns the OptionChains dict (refcount bump only).
    #[getter]
    fn option_chains(&self, py: Python<'_>) -> Py<PyOptionChains> {
        self.option_chains_obj.clone_ref(py)
    }

    /// LEAN API: `data.custom` — returns the CustomData dict (refcount bump only).
    #[getter]
    fn custom(&self, py: Python<'_>) -> Py<PyCustomData> {
        self.custom_data_obj.clone_ref(py)
    }

    /// LEAN API: `data.get(symbol)` — delegates to bars.get().
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        self.bars_obj.borrow(py).get(py, symbol)
    }

    /// LEAN API: `data.get_bar(symbol)` — alias for get().
    fn get_bar(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        self.get(py, symbol)
    }

    /// LEAN API: `data[symbol]`.
    fn __getitem__(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        match self.bars_obj.borrow(py).get(py, symbol) {
            Ok(v) => Ok(v),
            Err(_) => Ok(None),
        }
    }

    fn tickers(&self, py: Python<'_>) -> Vec<String> {
        self.bars_obj.borrow(py).ticker_to_sid.keys().cloned().collect()
    }

    fn __repr__(&self, py: Python<'_>) -> String {
        let n = self.bars_obj.borrow(py).bars.len();
        format!("Slice(bars={}, has_data={})", n, self.has_data)
    }
}

// ─── Custom Data ─────────────────────────────────────────────────────────────

/// Python-visible custom data point.
///
/// LEAN API: `data.custom["UNRATE"]` returns the latest `CustomDataPoint`
/// for the ticker.  Access via `.value`, `.time`, and `.fields`.
#[pyclass(name = "CustomDataPoint")]
#[derive(Debug, Clone)]
pub struct PyCustomDataPoint {
    /// Primary scalar value (equivalent to LEAN's `BaseData.Value`).
    #[pyo3(get)]
    pub value: f64,
    /// Date this point applies to.
    #[pyo3(get)]
    pub time: chrono::NaiveDate,
    /// JSON-decoded extra fields dict.
    fields_inner: HashMap<String, serde_json::Value>,
}

#[pymethods]
impl PyCustomDataPoint {
    /// Extra fields dict — `data.custom["VIX"].fields["open"]`.
    #[getter]
    fn fields(&self, py: Python<'_>) -> PyResult<PyObject> {
        use pyo3::types::PyDict;
        let dict = PyDict::new(py);
        for (k, v) in &self.fields_inner {
            let py_val = json_value_to_py(py, v)?;
            dict.set_item(k, py_val)?;
        }
        Ok(dict.into())
    }

    fn __repr__(&self) -> String {
        format!("CustomDataPoint(time={} value={})", self.time, self.value)
    }
}

/// Convert a `serde_json::Value` to a Python object.
fn json_value_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<PyObject> {
    match v {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => Ok(b.to_object(py)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.to_object(py))
            } else if let Some(f) = n.as_f64() {
                Ok(f.to_object(py))
            } else {
                Ok(n.to_string().to_object(py))
            }
        }
        serde_json::Value::String(s) => Ok(s.to_object(py)),
        serde_json::Value::Array(arr) => {
            use pyo3::types::PyList;
            let list = PyList::empty(py);
            for item in arr {
                list.append(json_value_to_py(py, item)?)?;
            }
            Ok(list.into())
        }
        serde_json::Value::Object(map) => {
            use pyo3::types::PyDict;
            let dict = PyDict::new(py);
            for (k, val) in map {
                dict.set_item(k, json_value_to_py(py, val)?)?;
            }
            Ok(dict.into())
        }
    }
}

/// LEAN API: `data.custom` — dict-like collection of custom data points.
///
/// Keyed by ticker string (e.g. `"UNRATE"`, `"VIX"`).
/// Each value is the latest `CustomDataPoint` for that ticker on this date.
#[pyclass(name = "CustomData")]
pub struct PyCustomData {
    /// ticker (uppercase) → latest data point for this bar
    points: HashMap<String, Py<PyCustomDataPoint>>,
}

impl PyCustomData {
    pub fn empty() -> Self {
        PyCustomData { points: HashMap::new() }
    }
}

#[pymethods]
impl PyCustomData {
    fn __getitem__(&self, py: Python<'_>, ticker: &str) -> PyResult<Option<Py<PyCustomDataPoint>>> {
        let key = ticker.to_uppercase();
        Ok(self.points.get(&key).map(|p| p.clone_ref(py)))
    }

    fn get(&self, py: Python<'_>, ticker: &str) -> PyResult<Option<Py<PyCustomDataPoint>>> {
        self.__getitem__(py, ticker)
    }

    fn __contains__(&self, ticker: &str) -> bool {
        self.points.contains_key(&ticker.to_uppercase())
    }

    fn __len__(&self) -> usize {
        self.points.len()
    }

    fn keys(&self) -> Vec<String> {
        self.points.keys().cloned().collect()
    }

    fn __repr__(&self) -> String {
        format!("CustomData(count={})", self.points.len())
    }
}

// ─── SliceProxy ───────────────────────────────────────────────────────────────

/// Pre-allocated Python objects for the simulation hot path.
///
/// Created once before the backtest loop.  Each iteration calls `update()` which
/// writes new OHLCV values directly into the pre-existing `PyTradeBar` objects
/// via `borrow_mut` — no Python allocation, no HashMap construction, no copies.
///
/// This mirrors Python.NET's proxy model: Python code receives a stable reference
/// to the same object each call; Rust mutates it between calls while the GIL is held.
pub struct SliceProxy {
    /// The `Slice` Python object passed to `on_data` each bar.
    pub py_slice: Py<PySlice>,
    /// Per-symbol mutable bar cells, keyed by symbol SID.
    bar_cells: HashMap<u64, Py<PyTradeBar>>,
    /// Per-symbol mutable quote bar cells, keyed by symbol SID.
    quote_bar_cells: HashMap<u64, Py<PyQuoteBar>>,
    /// The QuoteBars container object (shared with py_slice).
    quote_bars_cell: Py<PyQuoteBars>,
    /// Mutable option chains cell — updated in-place each bar.
    option_chains_cell: Py<PyOptionChains>,
    /// Mutable custom data cell — updated once per trading day.
    custom_data_cell: Py<PyCustomData>,
}

impl SliceProxy {
    /// Allocate one `PyTradeBar` per subscription.  One-time cost paid before
    /// the main loop; amortised over all trading days.
    pub fn new(py: Python<'_>, subscriptions: &[Arc<SubscriptionDataConfig>]) -> PyResult<Self> {
        let mut bar_cells:       HashMap<u64, Py<PyTradeBar>>  = HashMap::new();
        let mut bars_map:        HashMap<u64, Py<PyTradeBar>>  = HashMap::new();
        let mut quote_bar_cells: HashMap<u64, Py<PyQuoteBar>>  = HashMap::new();
        let mut quote_bars_map:  HashMap<u64, Py<PyQuoteBar>>  = HashMap::new();
        let mut ticker_to_sid:   HashMap<String, u64>          = HashMap::new();
        let mut qb_ticker_to_sid: HashMap<String, u64>         = HashMap::new();

        for sub in subscriptions {
            let sid = sub.symbol.id.sid;
            let py_bar = Py::new(py, PyTradeBar {
                open: 0.0, high: 0.0, low: 0.0, close: 0.0, volume: 0.0,
                symbol:   PySymbol { inner: sub.symbol.clone() },
                time:     chrono::NaiveDateTime::default(),
                end_time: chrono::NaiveDateTime::default(),
            })?;
            ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            ticker_to_sid.insert(sub.symbol.permtick.clone(), sid);
            bars_map.insert(sid, py_bar.clone_ref(py));
            bar_cells.insert(sid, py_bar);

            let py_qbar = Py::new(py, PyQuoteBar {
                bid_open: 0.0, bid_high: 0.0, bid_low: 0.0, bid_close: 0.0,
                ask_open: 0.0, ask_high: 0.0, ask_low: 0.0, ask_close: 0.0,
                bid_size: 0.0, ask_size: 0.0,
                symbol:   PySymbol { inner: sub.symbol.clone() },
                time:     chrono::NaiveDateTime::default(),
                end_time: chrono::NaiveDateTime::default(),
            })?;
            qb_ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            qb_ticker_to_sid.insert(sub.symbol.permtick.clone(), sid);
            quote_bars_map.insert(sid, py_qbar.clone_ref(py));
            quote_bar_cells.insert(sid, py_qbar);
        }

        let py_bars      = Py::new(py, PyTradeBars { bars: bars_map, ticker_to_sid })?;
        let py_chains    = Py::new(py, PyOptionChains::empty())?;
        let py_qbars_obj = Py::new(py, PyQuoteBars { bars: quote_bars_map, ticker_to_sid: qb_ticker_to_sid })?;
        let py_custom    = Py::new(py, PyCustomData::empty())?;
        let py_slice     = Py::new(py, PySlice {
            bars_obj:        py_bars,
            quote_bars_obj:  py_qbars_obj.clone_ref(py),
            option_chains_obj: py_chains.clone_ref(py),
            custom_data_obj: py_custom.clone_ref(py),
            has_data: false,
        })?;

        Ok(SliceProxy {
            py_slice,
            bar_cells,
            quote_bar_cells,
            quote_bars_cell: py_qbars_obj,
            option_chains_cell: py_chains,
            custom_data_cell: py_custom,
        })
    }

    /// Write new bar values in-place.  Zero allocation; ~5 f64 writes + 2 string
    /// formats per symbol.  Must be called with the GIL held and no active Python
    /// borrows on the bar objects (guaranteed safe between `on_data` calls).
    pub fn update(&self, py: Python<'_>, slice: &Slice) {
        for (&sid, bar) in &slice.bars {
            if let Some(py_bar) = self.bar_cells.get(&sid) {
                let mut b  = py_bar.borrow_mut(py);
                b.open     = bar.open.to_f64().unwrap_or(0.0);
                b.high     = bar.high.to_f64().unwrap_or(0.0);
                b.low      = bar.low.to_f64().unwrap_or(0.0);
                b.close    = bar.close.to_f64().unwrap_or(0.0);
                b.volume   = bar.volume.to_f64().unwrap_or(0.0);
                b.time     = ns_to_naive(bar.time.0);
                b.end_time = ns_to_naive(bar.end_time.0);
            }
        }
        self.py_slice.borrow_mut(py).has_data = slice.has_data;
    }

    /// Write new quote bar values in-place for a set of bars.
    /// Zero allocation on the hot path; updates only the bars present in `quote_bars`.
    /// Also clears the QuoteBars container and re-populates it with only the provided SIDs.
    pub fn update_quote_bars(&self, py: Python<'_>, quote_bars: &HashMap<u64, QuoteBar>) {
        use rust_decimal::prelude::ToPrimitive;
        let to_f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);

        // Update in-place cells for symbols that have quote bars.
        for (&sid, qbar) in quote_bars {
            if let Some(py_qbar) = self.quote_bar_cells.get(&sid) {
                let mut b = py_qbar.borrow_mut(py);
                b.bid_open  = qbar.bid.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
                b.bid_high  = qbar.bid.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
                b.bid_low   = qbar.bid.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
                b.bid_close = qbar.bid.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
                b.ask_open  = qbar.ask.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
                b.ask_high  = qbar.ask.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
                b.ask_low   = qbar.ask.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
                b.ask_close = qbar.ask.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
                b.bid_size  = to_f(qbar.last_bid_size);
                b.ask_size  = to_f(qbar.last_ask_size);
                b.time      = ns_to_naive(qbar.time.0);
                b.end_time  = ns_to_naive(qbar.end_time.0);
            }
        }

        // Update the QuoteBars container to only expose SIDs with data this minute.
        {
            let mut qbars_obj = self.quote_bars_cell.borrow_mut(py);
            qbars_obj.bars.clear();
            for &sid in quote_bars.keys() {
                if let Some(cell) = self.quote_bar_cells.get(&sid) {
                    qbars_obj.bars.insert(sid, cell.clone_ref(py));
                }
            }
        }
    }

    /// Write the option chains for this bar in-place.
    /// Called once per trading day before `on_data` when option subscriptions exist.
    pub fn update_option_chains(&self, py: Python<'_>, chains: &[(String, OptionChain)]) {
        let mut chains_obj = self.option_chains_cell.borrow_mut(py);
        chains_obj.clear();
        for (permtick, chain) in chains {
            let py_chain = PyOptionChain { inner: chain.clone() };
            chains_obj.set(py, permtick, py_chain).ok();
        }
    }

    /// Write custom data points for this bar in-place.
    ///
    /// Replaces the `data.custom` dict with the latest points for each ticker.
    /// Called once per trading day (or once per minute in minute-mode) before `on_data`.
    ///
    /// `data`: ticker (any case) → list of `CustomDataPoint`s for this date.
    /// The last point in each list is used as the representative value.
    pub fn update_custom_data(
        &self,
        py: Python<'_>,
        data: &HashMap<String, Vec<CustomDataPoint>>,
    ) {
        let mut custom_obj = self.custom_data_cell.borrow_mut(py);
        custom_obj.points.clear();

        for (ticker, points) in data {
            // Use the last point as the current value (most recent intraday bar,
            // or the single daily point).
            let Some(last) = points.last() else { continue; };
            let py_point = match Py::new(py, PyCustomDataPoint {
                value: last.value.to_f64().unwrap_or(0.0),
                time: last.time,
                fields_inner: last.fields.clone(),
            }) {
                Ok(p) => p,
                Err(_) => continue,
            };
            custom_obj.points.insert(ticker.to_uppercase(), py_point);
        }
    }
}
