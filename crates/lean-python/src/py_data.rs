use crate::py_options::PyOptionChains;
use crate::py_types::{PySecurity, PySymbol};
use lean_core::TickType;
use lean_data::{
    CustomDataPoint, Delisting, DelistingType, Slice, SubscriptionDataConfig, SymbolChangedEvent,
    Tick, TradeBar,
};
use lean_data::{MarginInterestRate, PerpetualContext, QuoteBar};
use lean_options::OptionChain;
use pyo3::prelude::*;
use pyo3::IntoPyObjectExt;
use rust_decimal::prelude::ToPrimitive;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

fn ns_to_naive(ns: i64) -> chrono::NaiveDateTime {
    thread_local! {
        static NS_TO_NAIVE_CACHE: RefCell<HashMap<i64, chrono::NaiveDateTime>> =
            RefCell::new(HashMap::new());
    }

    if let Some(cached) = NS_TO_NAIVE_CACHE.with(|cache| cache.borrow().get(&ns).copied()) {
        return cached;
    }

    use chrono::{DateTime as ChronoDateTime, Utc};
    use chrono_tz::US::Eastern;
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    // Deliver bar times in Eastern Time (exchange local), matching LEAN's behavior.
    let naive = dt.with_timezone(&Eastern).naive_local();
    NS_TO_NAIVE_CACHE.with(|cache| {
        cache.borrow_mut().insert(ns, naive);
    });
    naive
}

/// Python-visible TradeBar.
///
/// Not `frozen` — Rust mutates fields in-place via `SliceProxy::update` each bar,
/// eliminating all per-day allocation.  Python only gets read-only `#[pyo3(get)]`
/// accessors, so strategies cannot accidentally overwrite bar data.
#[pyclass(name = "TradeBar")]
#[derive(Debug, Clone)]
pub struct PyTradeBar {
    #[pyo3(get)]
    pub open: f64,
    #[pyo3(get)]
    pub high: f64,
    #[pyo3(get)]
    pub low: f64,
    #[pyo3(get)]
    pub close: f64,
    #[pyo3(get)]
    pub volume: f64,
    #[pyo3(get)]
    pub symbol: PySymbol,
    /// Bar open time as a datetime. Matches LEAN's `TradeBar.Time`.
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    /// Bar close time as a datetime. Matches LEAN's `TradeBar.EndTime`.
    #[pyo3(get)]
    pub end_time: chrono::NaiveDateTime,
}

impl From<&TradeBar> for PyTradeBar {
    fn from(b: &TradeBar) -> Self {
        PyTradeBar {
            open: b.open.to_f64().unwrap_or(0.0),
            high: b.high.to_f64().unwrap_or(0.0),
            low: b.low.to_f64().unwrap_or(0.0),
            close: b.close.to_f64().unwrap_or(0.0),
            volume: b.volume.to_f64().unwrap_or(0.0),
            symbol: PySymbol {
                inner: b.symbol.clone(),
            },
            time: ns_to_naive(b.time.0),
            end_time: ns_to_naive(b.end_time.0),
        }
    }
}

#[pymethods]
impl PyTradeBar {
    /// `Value` matches C# `BaseData.Value` which returns `Close` for TradeBar.
    #[getter]
    fn value(&self) -> f64 {
        self.close
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'TradeBar' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "TradeBar({} O={:.2} H={:.2} L={:.2} C={:.2} V={:.0})",
            self.symbol.inner.value, self.open, self.high, self.low, self.close, self.volume
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
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return Ok(self.ticker_to_sid.get(&ticker).copied());
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or str",
        ))
    }
}

#[pymethods]
impl PyTradeBars {
    /// Returns a Python reference to the bar — no data copied.
    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyTradeBar>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.bars.get(&sid).map(|b| b.clone_ref(py))))
    }

    /// LEAN API: `data.bars.get(symbol)` — returns None if not present.
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.bars.get(&sid).map(|b| b.clone_ref(py))))
    }

    fn __len__(&self) -> usize {
        self.bars.len()
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.bars.contains_key(&sid))
            .unwrap_or(false))
    }

    fn values(&self, py: Python<'_>) -> Vec<Py<PyTradeBar>> {
        self.bars.values().map(|b| b.clone_ref(py)).collect()
    }

    /// LEAN C# API: `data.Bars.ContainsKey(symbol)` — alias for `__contains__`.
    fn contains_key(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.__contains__(symbol)
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'TradeBars' object has no attribute '{name}'"
        )))
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
    #[pyo3(get)]
    pub open: f64,
    #[pyo3(get)]
    pub high: f64,
    #[pyo3(get)]
    pub low: f64,
    #[pyo3(get)]
    pub close: f64,
}

#[pymethods]
impl PyBar {
    fn __repr__(&self) -> String {
        format!(
            "Bar(O={:.4} H={:.4} L={:.4} C={:.4})",
            self.open, self.high, self.low, self.close
        )
    }
}

/// Python-visible QuoteBar (bid/ask OHLC).
/// LEAN API: `data.quote_bars[symbol]` → QuoteBar
#[pyclass(name = "QuoteBar")]
#[derive(Debug, Clone)]
pub struct PyQuoteBar {
    #[pyo3(get)]
    pub bid_open: f64,
    #[pyo3(get)]
    pub bid_high: f64,
    #[pyo3(get)]
    pub bid_low: f64,
    #[pyo3(get)]
    pub bid_close: f64,
    #[pyo3(get)]
    pub ask_open: f64,
    #[pyo3(get)]
    pub ask_high: f64,
    #[pyo3(get)]
    pub ask_low: f64,
    #[pyo3(get)]
    pub ask_close: f64,
    #[pyo3(get)]
    pub bid_size: f64,
    #[pyo3(get)]
    pub ask_size: f64,
    #[pyo3(get)]
    pub symbol: PySymbol,
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub end_time: chrono::NaiveDateTime,
}

#[pymethods]
impl PyQuoteBar {
    /// LEAN API: bar.close → mid-close price
    #[getter]
    fn close(&self) -> f64 {
        (self.bid_close + self.ask_close) / 2.0
    }
    /// LEAN API: bar.open → mid-open price
    #[getter]
    fn open(&self) -> f64 {
        (self.bid_open + self.ask_open) / 2.0
    }
    /// LEAN API: qb.bid → Bar(open, high, low, close) for bid side
    #[getter]
    fn bid(&self, py: Python<'_>) -> PyResult<Py<PyBar>> {
        Py::new(
            py,
            PyBar {
                open: self.bid_open,
                high: self.bid_high,
                low: self.bid_low,
                close: self.bid_close,
            },
        )
    }
    /// LEAN API: qb.ask → Bar(open, high, low, close) for ask side
    #[getter]
    fn ask(&self, py: Python<'_>) -> PyResult<Py<PyBar>> {
        Py::new(
            py,
            PyBar {
                open: self.ask_open,
                high: self.ask_high,
                low: self.ask_low,
                close: self.ask_close,
            },
        )
    }
    fn __repr__(&self) -> String {
        format!(
            "QuoteBar({} bid={:.4} ask={:.4})",
            self.symbol.inner.value, self.bid_close, self.ask_close
        )
    }
}

impl From<&QuoteBar> for PyQuoteBar {
    fn from(q: &QuoteBar) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        let to_f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);
        let bid_open = q.bid.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
        let bid_high = q.bid.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
        let bid_low = q.bid.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
        let bid_close = q.bid.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
        let ask_open = q.ask.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
        let ask_high = q.ask.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
        let ask_low = q.ask.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
        let ask_close = q.ask.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
        PyQuoteBar {
            bid_open,
            bid_high,
            bid_low,
            bid_close,
            ask_open,
            ask_high,
            ask_low,
            ask_close,
            bid_size: to_f(q.last_bid_size),
            ask_size: to_f(q.last_ask_size),
            symbol: PySymbol {
                inner: q.symbol.clone(),
            },
            time: ns_to_naive(q.time.0),
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
        PyQuoteBars {
            bars: HashMap::new(),
            ticker_to_sid: HashMap::new(),
        }
    }

    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return Ok(self.ticker_to_sid.get(&ticker).copied());
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or str",
        ))
    }
}

#[pymethods]
impl PyQuoteBars {
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyQuoteBar>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.bars.get(&sid).map(|b| b.clone_ref(py))))
    }
    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyQuoteBar>>> {
        self.get(py, symbol)
    }
    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.bars.contains_key(&sid))
            .unwrap_or(false))
    }
    fn __len__(&self) -> usize {
        self.bars.len()
    }
    fn values(&self, py: Python<'_>) -> Vec<Py<PyQuoteBar>> {
        self.bars.values().map(|b| b.clone_ref(py)).collect()
    }
    fn __repr__(&self) -> String {
        format!("QuoteBars(count={})", self.bars.len())
    }
}

/// Python-visible MarginInterestRate.
#[pyclass(name = "MarginInterestRate")]
#[derive(Debug, Clone)]
pub struct PyMarginInterestRate {
    #[pyo3(get)]
    pub symbol: PySymbol,
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub interest_rate: f64,
}

impl From<&MarginInterestRate> for PyMarginInterestRate {
    fn from(rate: &MarginInterestRate) -> Self {
        PyMarginInterestRate {
            symbol: PySymbol {
                inner: rate.symbol.clone(),
            },
            time: ns_to_naive(rate.time.0),
            interest_rate: rate.interest_rate.to_f64().unwrap_or(0.0),
        }
    }
}

#[pymethods]
impl PyMarginInterestRate {
    #[getter]
    fn value(&self) -> f64 {
        self.interest_rate
    }

    fn __repr__(&self) -> String {
        format!(
            "MarginInterestRate({} rate={:.8})",
            self.symbol.inner.value, self.interest_rate
        )
    }
}

/// LEAN API: `data.margin_interest_rates` — dict-like funding-rate collection.
#[pyclass(name = "MarginInterestRates")]
pub struct PyMarginInterestRates {
    rates: HashMap<u64, Py<PyMarginInterestRate>>,
    ticker_to_sid: HashMap<String, u64>,
}

impl PyMarginInterestRates {
    pub fn empty() -> Self {
        PyMarginInterestRates {
            rates: HashMap::new(),
            ticker_to_sid: HashMap::new(),
        }
    }

    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return Ok(self.ticker_to_sid.get(&ticker).copied());
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or str",
        ))
    }
}

#[pymethods]
impl PyMarginInterestRates {
    fn get(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyMarginInterestRate>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.rates.get(&sid).map(|r| r.clone_ref(py))))
    }

    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyMarginInterestRate>>> {
        self.get(py, symbol)
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.rates.contains_key(&sid))
            .unwrap_or(false))
    }

    fn __len__(&self) -> usize {
        self.rates.len()
    }

    fn values(&self, py: Python<'_>) -> Vec<Py<PyMarginInterestRate>> {
        self.rates.values().map(|r| r.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        format!("MarginInterestRates(count={})", self.rates.len())
    }
}

/// Python-visible PerpetualContext.
#[pyclass(name = "PerpetualContext")]
#[derive(Debug, Clone)]
pub struct PyPerpetualContext {
    #[pyo3(get)]
    pub symbol: PySymbol,
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub end_time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub funding: f64,
    #[pyo3(get)]
    pub open_interest: f64,
    #[pyo3(get)]
    pub prev_day_px: f64,
    #[pyo3(get)]
    pub day_ntl_vlm: f64,
    #[pyo3(get)]
    pub premium: f64,
    #[pyo3(get)]
    pub oracle_px: f64,
    #[pyo3(get)]
    pub mark_px: f64,
    #[pyo3(get)]
    pub mid_px: f64,
    #[pyo3(get)]
    pub impact_bid_px: f64,
    #[pyo3(get)]
    pub impact_ask_px: f64,
}

impl From<&PerpetualContext> for PyPerpetualContext {
    fn from(context: &PerpetualContext) -> Self {
        let to_f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);
        Self {
            symbol: PySymbol {
                inner: context.symbol.clone(),
            },
            time: ns_to_naive(context.time.0),
            end_time: ns_to_naive(context.end_time.0),
            funding: to_f(context.funding),
            open_interest: to_f(context.open_interest),
            prev_day_px: to_f(context.prev_day_px),
            day_ntl_vlm: to_f(context.day_ntl_vlm),
            premium: to_f(context.premium),
            oracle_px: to_f(context.oracle_px),
            mark_px: to_f(context.mark_px),
            mid_px: to_f(context.mid_px),
            impact_bid_px: to_f(context.impact_bid_px),
            impact_ask_px: to_f(context.impact_ask_px),
        }
    }
}

#[pymethods]
impl PyPerpetualContext {
    #[getter]
    fn value(&self) -> f64 {
        self.mark_px
    }

    fn __repr__(&self) -> String {
        format!(
            "PerpetualContext({} mark={:.4} mid={:.4} oi={:.4})",
            self.symbol.inner.value, self.mark_px, self.mid_px, self.open_interest
        )
    }
}

/// LEAN-style dict-like collection for perpetual context rows.
#[pyclass(name = "PerpetualContexts")]
pub struct PyPerpetualContexts {
    contexts: HashMap<u64, Py<PyPerpetualContext>>,
    ticker_to_sid: HashMap<String, u64>,
}

impl PyPerpetualContexts {
    pub fn empty() -> Self {
        Self {
            contexts: HashMap::new(),
            ticker_to_sid: HashMap::new(),
        }
    }

    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return Ok(self.ticker_to_sid.get(&ticker).copied());
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or str",
        ))
    }
}

#[pymethods]
impl PyPerpetualContexts {
    fn get(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyPerpetualContext>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.contexts.get(&sid).map(|c| c.clone_ref(py))))
    }

    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyPerpetualContext>>> {
        self.get(py, symbol)
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.contexts.contains_key(&sid))
            .unwrap_or(false))
    }

    fn __len__(&self) -> usize {
        self.contexts.len()
    }

    fn values(&self, py: Python<'_>) -> Vec<Py<PyPerpetualContext>> {
        self.contexts.values().map(|c| c.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        format!("PerpetualContexts(count={})", self.contexts.len())
    }
}

/// Python-visible Tick.
#[pyclass(name = "Tick")]
#[derive(Debug, Clone)]
pub struct PyTick {
    #[pyo3(get)]
    pub symbol: PySymbol,
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub value: f64,
    #[pyo3(get)]
    pub quantity: f64,
    #[pyo3(get)]
    pub bid_price: f64,
    #[pyo3(get)]
    pub ask_price: f64,
    #[pyo3(get)]
    pub bid_size: f64,
    #[pyo3(get)]
    pub ask_size: f64,
    #[pyo3(get)]
    pub exchange: Option<String>,
    #[pyo3(get)]
    pub sale_condition: Option<String>,
    #[pyo3(get)]
    pub suspicious: bool,
    tick_type: TickType,
}

impl From<&Tick> for PyTick {
    fn from(tick: &Tick) -> Self {
        PyTick {
            symbol: PySymbol {
                inner: tick.symbol.clone(),
            },
            time: ns_to_naive(tick.time.0),
            value: tick.value.to_f64().unwrap_or(0.0),
            quantity: tick.quantity.to_f64().unwrap_or(0.0),
            bid_price: tick.bid_price.to_f64().unwrap_or(0.0),
            ask_price: tick.ask_price.to_f64().unwrap_or(0.0),
            bid_size: tick.bid_size.to_f64().unwrap_or(0.0),
            ask_size: tick.ask_size.to_f64().unwrap_or(0.0),
            exchange: tick.exchange.clone(),
            sale_condition: tick.sale_condition.clone(),
            suspicious: tick.suspicious,
            tick_type: tick.tick_type,
        }
    }
}

#[pymethods]
impl PyTick {
    #[getter]
    fn tick_type(&self) -> &str {
        match self.tick_type {
            TickType::Trade => "Trade",
            TickType::Quote => "Quote",
            TickType::OpenInterest => "OpenInterest",
        }
    }

    fn is_trade(&self) -> bool {
        self.tick_type == TickType::Trade
    }

    fn is_quote(&self) -> bool {
        self.tick_type == TickType::Quote
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Tick' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "Tick({} type={} value={:.4})",
            self.symbol.inner.value,
            self.tick_type(),
            self.value
        )
    }
}

/// LEAN API: `data.ticks` — dict-like tick collection keyed by symbol.
#[pyclass(name = "Ticks")]
pub struct PyTicks {
    ticks: HashMap<u64, Vec<Py<PyTick>>>,
    ticker_to_sid: HashMap<String, u64>,
}

impl PyTicks {
    pub fn empty() -> Self {
        PyTicks {
            ticks: HashMap::new(),
            ticker_to_sid: HashMap::new(),
        }
    }

    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        if let Ok(ticker) = arg.extract::<String>() {
            return Ok(self.ticker_to_sid.get(&ticker).copied());
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or str",
        ))
    }
}

#[pymethods]
impl PyTicks {
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Vec<Py<PyTick>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.ticks.get(&sid))
            .map(|ticks| ticks.iter().map(|t| t.clone_ref(py)).collect())
            .unwrap_or_default())
    }

    fn __getitem__(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Vec<Py<PyTick>>> {
        self.get(py, symbol)
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.ticks.contains_key(&sid))
            .unwrap_or(false))
    }

    fn __len__(&self) -> usize {
        self.ticks.len()
    }

    fn values(&self, py: Python<'_>) -> Vec<Vec<Py<PyTick>>> {
        self.ticks
            .values()
            .map(|ticks| ticks.iter().map(|t| t.clone_ref(py)).collect())
            .collect()
    }

    fn __repr__(&self) -> String {
        format!("Ticks(count={})", self.ticks.len())
    }
}

/// Python-visible Delisting event.
///
/// LEAN API: `data.Delistings[symbol]` → Delisting
#[pyclass(name = "Delisting")]
#[derive(Debug, Clone)]
pub struct PyDelisting {
    #[pyo3(get)]
    pub symbol: PySymbol,
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub price: f64,
    delisting_type: DelistingType,
}

impl From<&Delisting> for PyDelisting {
    fn from(d: &Delisting) -> Self {
        PyDelisting {
            symbol: PySymbol {
                inner: d.symbol.clone(),
            },
            time: ns_to_naive(d.time.0),
            price: d.price.to_f64().unwrap_or(0.0),
            delisting_type: d.delisting_type,
        }
    }
}

#[pymethods]
impl PyDelisting {
    /// LEAN API: `delisting.Type` → "Warning" or "Delisted"
    #[getter(r#type)]
    fn delisting_type_str(&self) -> &str {
        match self.delisting_type {
            DelistingType::Warning => "Warning",
            DelistingType::Delisted => "Delisted",
        }
    }

    fn is_warning(&self) -> bool {
        self.delisting_type == DelistingType::Warning
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Delisting' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "Delisting({} type={} price={:.2})",
            self.symbol.inner.value,
            self.delisting_type_str(),
            self.price
        )
    }
}

/// LEAN API: `data.Delistings` — dict-like collection of delisting events.
#[pyclass(name = "Delistings")]
pub struct PyDelistings {
    events: HashMap<u64, Py<PyDelisting>>,
}

impl PyDelistings {
    pub fn empty() -> Self {
        PyDelistings {
            events: HashMap::new(),
        }
    }

    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security or Symbol",
        ))
    }
}

#[pymethods]
impl PyDelistings {
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyDelisting>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.events.get(&sid).map(|e| e.clone_ref(py))))
    }

    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyDelisting>>> {
        self.get(py, symbol)
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.events.contains_key(&sid))
            .unwrap_or(false))
    }

    fn __len__(&self) -> usize {
        self.events.len()
    }

    fn values(&self, py: Python<'_>) -> Vec<Py<PyDelisting>> {
        self.events.values().map(|e| e.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        format!("Delistings(count={})", self.events.len())
    }
}

/// Python-visible SymbolChangedEvent.
///
/// LEAN API: `data.SymbolChangedEvents[symbol]` → SymbolChangedEvent
#[pyclass(name = "SymbolChangedEvent")]
#[derive(Debug, Clone)]
pub struct PySymbolChangedEvent {
    #[pyo3(get)]
    pub symbol: PySymbol,
    #[pyo3(get)]
    pub time: chrono::NaiveDateTime,
    #[pyo3(get)]
    pub old_symbol: String,
    #[pyo3(get)]
    pub new_symbol: String,
}

impl From<&SymbolChangedEvent> for PySymbolChangedEvent {
    fn from(ev: &SymbolChangedEvent) -> Self {
        PySymbolChangedEvent {
            symbol: PySymbol {
                inner: ev.symbol.clone(),
            },
            time: ns_to_naive(ev.time.0),
            old_symbol: ev.old_symbol.clone(),
            new_symbol: ev.new_symbol.clone(),
        }
    }
}

#[pymethods]
impl PySymbolChangedEvent {
    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'SymbolChangedEvent' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "SymbolChangedEvent({} → {})",
            self.old_symbol, self.new_symbol
        )
    }
}

/// LEAN API: `data.SymbolChangedEvents` — dict-like collection of rename events.
#[pyclass(name = "SymbolChangedEvents")]
pub struct PySymbolChangedEvents {
    events: HashMap<u64, Py<PySymbolChangedEvent>>,
}

impl PySymbolChangedEvents {
    pub fn empty() -> Self {
        PySymbolChangedEvents {
            events: HashMap::new(),
        }
    }

    fn resolve_sid(&self, arg: &Bound<'_, PyAny>) -> PyResult<Option<u64>> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(Some(sym.get().inner.id.sid));
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(Some(sec.get().inner.inner.id.sid));
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security or Symbol",
        ))
    }
}

#[pymethods]
impl PySymbolChangedEvents {
    fn get(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PySymbolChangedEvent>>> {
        Ok(self
            .resolve_sid(symbol)?
            .and_then(|sid| self.events.get(&sid).map(|e| e.clone_ref(py))))
    }

    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PySymbolChangedEvent>>> {
        self.get(py, symbol)
    }

    fn __contains__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        Ok(self
            .resolve_sid(symbol)?
            .map(|sid| self.events.contains_key(&sid))
            .unwrap_or(false))
    }

    fn __len__(&self) -> usize {
        self.events.len()
    }

    fn values(&self, py: Python<'_>) -> Vec<Py<PySymbolChangedEvent>> {
        self.events.values().map(|e| e.clone_ref(py)).collect()
    }

    fn __repr__(&self) -> String {
        format!("SymbolChangedEvents(count={})", self.events.len())
    }
}

/// Python-visible Slice — the object delivered to `on_data`.
///
/// Holds a `Py<PyTradeBars>` reference rather than owning bar data, so the
/// `bars` getter is a single refcount bump (O(1)) instead of a full HashMap clone.
#[pyclass(name = "Slice")]
pub struct PySlice {
    bars_obj: Py<PyTradeBars>,
    quote_bars_obj: Py<PyQuoteBars>,
    margin_interest_rates_obj: Py<PyMarginInterestRates>,
    perpetual_contexts_obj: Py<PyPerpetualContexts>,
    ticks_obj: Py<PyTicks>,
    option_chains_obj: Py<PyOptionChains>,
    custom_data_obj: Py<PyCustomData>,
    delistings_obj: Py<PyDelistings>,
    symbol_changed_events_obj: Py<PySymbolChangedEvents>,
    #[pyo3(get)]
    pub has_data: bool,
}

impl PySlice {
    /// Build a self-contained Slice from a Rust Slice.
    /// Used for warmup and tests where no SliceProxy is available.
    pub fn from_slice(py: Python<'_>, slice: &Slice) -> PyResult<Self> {
        Self::from_slice_with_custom(py, slice, &slice.custom_data)
    }

    pub fn from_slice_with_custom(
        py: Python<'_>,
        slice: &Slice,
        custom_data: &HashMap<String, Vec<CustomDataPoint>>,
    ) -> PyResult<Self> {
        let mut bars: HashMap<u64, Py<PyTradeBar>> = HashMap::with_capacity(slice.bars.len());
        let mut ticker_to_sid: HashMap<String, u64> =
            HashMap::with_capacity(slice.bars.len().saturating_mul(2));
        for (&sid, bar) in &slice.bars {
            let py_bar = Py::new(py, PyTradeBar::from(bar))?;
            ticker_to_sid.insert(bar.symbol.value.clone(), sid);
            ticker_to_sid.insert(bar.symbol.permtick.clone(), sid);
            bars.insert(sid, py_bar);
        }
        let py_bars = Py::new(
            py,
            PyTradeBars {
                bars,
                ticker_to_sid,
            },
        )?;
        let py_chains = Py::new(py, PyOptionChains::empty())?;
        let mut py_quote_map: HashMap<u64, Py<PyQuoteBar>> =
            HashMap::with_capacity(slice.quote_bars.len());
        let mut quote_ticker_to_sid: HashMap<String, u64> =
            HashMap::with_capacity(slice.quote_bars.len().saturating_mul(2));
        for (&sid, bar) in &slice.quote_bars {
            let py_bar = Py::new(py, PyQuoteBar::from(bar))?;
            quote_ticker_to_sid.insert(bar.symbol.value.clone(), sid);
            quote_ticker_to_sid.insert(bar.symbol.permtick.clone(), sid);
            py_quote_map.insert(sid, py_bar);
        }
        let py_quote_bars = Py::new(
            py,
            PyQuoteBars {
                bars: py_quote_map,
                ticker_to_sid: quote_ticker_to_sid,
            },
        )?;
        let mut py_margin_rate_map: HashMap<u64, Py<PyMarginInterestRate>> =
            HashMap::with_capacity(slice.margin_interest_rates.len());
        let mut margin_ticker_to_sid: HashMap<String, u64> =
            HashMap::with_capacity(slice.margin_interest_rates.len().saturating_mul(2));
        for (&sid, rate) in &slice.margin_interest_rates {
            let py_rate = Py::new(py, PyMarginInterestRate::from(rate))?;
            margin_ticker_to_sid.insert(rate.symbol.value.clone(), sid);
            margin_ticker_to_sid.insert(rate.symbol.permtick.clone(), sid);
            py_margin_rate_map.insert(sid, py_rate);
        }
        let py_margin_interest_rates = Py::new(
            py,
            PyMarginInterestRates {
                rates: py_margin_rate_map,
                ticker_to_sid: margin_ticker_to_sid,
            },
        )?;
        let mut py_perpetual_context_map: HashMap<u64, Py<PyPerpetualContext>> =
            HashMap::with_capacity(slice.perpetual_contexts.len());
        let mut context_ticker_to_sid: HashMap<String, u64> =
            HashMap::with_capacity(slice.perpetual_contexts.len().saturating_mul(2));
        for (&sid, context) in &slice.perpetual_contexts {
            let py_context = Py::new(py, PyPerpetualContext::from(context))?;
            context_ticker_to_sid.insert(context.symbol.value.clone(), sid);
            context_ticker_to_sid.insert(context.symbol.permtick.clone(), sid);
            py_perpetual_context_map.insert(sid, py_context);
        }
        let py_perpetual_contexts = Py::new(
            py,
            PyPerpetualContexts {
                contexts: py_perpetual_context_map,
                ticker_to_sid: context_ticker_to_sid,
            },
        )?;
        let mut py_ticks_map: HashMap<u64, Vec<Py<PyTick>>> =
            HashMap::with_capacity(slice.ticks.len());
        let mut tick_ticker_to_sid: HashMap<String, u64> =
            HashMap::with_capacity(slice.ticks.len().saturating_mul(2));
        for (&sid, ticks) in &slice.ticks {
            let Some(first) = ticks.first() else {
                continue;
            };
            tick_ticker_to_sid.insert(first.symbol.value.clone(), sid);
            tick_ticker_to_sid.insert(first.symbol.permtick.clone(), sid);
            let mut py_ticks = Vec::with_capacity(ticks.len());
            for tick in ticks {
                py_ticks.push(Py::new(py, PyTick::from(tick))?);
            }
            py_ticks_map.insert(sid, py_ticks);
        }
        let py_ticks = Py::new(
            py,
            PyTicks {
                ticks: py_ticks_map,
                ticker_to_sid: tick_ticker_to_sid,
            },
        )?;
        let py_custom = Py::new(py, PyCustomData::from_points(py, custom_data)?)?;
        let py_delistings = {
            let mut events: HashMap<u64, Py<PyDelisting>> =
                HashMap::with_capacity(slice.delistings.len());
            for (&sid, d) in &slice.delistings {
                events.insert(sid, Py::new(py, PyDelisting::from(d))?);
            }
            Py::new(py, PyDelistings { events })?
        };
        let py_sce = {
            let mut events: HashMap<u64, Py<PySymbolChangedEvent>> =
                HashMap::with_capacity(slice.symbol_changed_events.len());
            for (&sid, ev) in &slice.symbol_changed_events {
                events.insert(sid, Py::new(py, PySymbolChangedEvent::from(ev))?);
            }
            Py::new(py, PySymbolChangedEvents { events })?
        };
        Ok(PySlice {
            bars_obj: py_bars,
            quote_bars_obj: py_quote_bars,
            margin_interest_rates_obj: py_margin_interest_rates,
            perpetual_contexts_obj: py_perpetual_contexts,
            ticks_obj: py_ticks,
            option_chains_obj: py_chains,
            custom_data_obj: py_custom,
            delistings_obj: py_delistings,
            symbol_changed_events_obj: py_sce,
            has_data: slice.has_data || !custom_data.is_empty(),
        })
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

    /// LEAN API: `data.margin_interest_rates`.
    #[getter]
    fn margin_interest_rates(&self, py: Python<'_>) -> Py<PyMarginInterestRates> {
        self.margin_interest_rates_obj.clone_ref(py)
    }

    /// LEAN-style perpetual context collection.
    #[getter]
    fn perpetual_contexts(&self, py: Python<'_>) -> Py<PyPerpetualContexts> {
        self.perpetual_contexts_obj.clone_ref(py)
    }

    /// LEAN API: `data.ticks` — returns the Ticks collection (refcount bump only).
    #[getter]
    fn ticks(&self, py: Python<'_>) -> Py<PyTicks> {
        self.ticks_obj.clone_ref(py)
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

    /// LEAN API: `data.Delistings` / `data.delistings` — returns the Delistings dict.
    #[getter]
    fn delistings(&self, py: Python<'_>) -> Py<PyDelistings> {
        self.delistings_obj.clone_ref(py)
    }

    /// LEAN API: `data.SymbolChangedEvents` / `data.symbol_changed_events`.
    #[getter]
    fn symbol_changed_events(&self, py: Python<'_>) -> Py<PySymbolChangedEvents> {
        self.symbol_changed_events_obj.clone_ref(py)
    }

    /// LEAN API: `data.get(symbol)` — delegates to bars.get().
    fn get(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<Option<Py<PyTradeBar>>> {
        self.bars_obj.borrow(py).get(py, symbol)
    }

    /// LEAN API: `data.get_bar(symbol)` — alias for get().
    fn get_bar(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyTradeBar>>> {
        self.get(py, symbol)
    }

    /// LEAN API: `data[symbol]`.
    fn __getitem__(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Option<Py<PyTradeBar>>> {
        match self.bars_obj.borrow(py).get(py, symbol) {
            Ok(v) => Ok(v),
            Err(_) => Ok(None),
        }
    }

    /// LEAN API: `symbol in data` — true if this slice has a bar for `symbol`.
    ///
    /// Without this, Python falls back to the legacy `__getitem__(0, 1, 2 …)`
    /// sequence protocol.  `__getitem__` returns `Ok(None)` rather than raising
    /// `IndexError`, so Python never terminates the loop → 100 % CPU spin.
    fn __contains__(&self, py: Python<'_>, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.bars_obj.borrow(py).__contains__(symbol)
    }

    fn tickers(&self, py: Python<'_>) -> Vec<String> {
        self.bars_obj
            .borrow(py)
            .ticker_to_sid
            .keys()
            .cloned()
            .collect()
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Slice' object has no attribute '{name}'"
        )))
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
    /// Emission/end time in algorithm/exchange local time when available.
    #[pyo3(get)]
    pub end_time: chrono::NaiveDateTime,
    /// JSON-decoded extra fields dict.
    fields_inner: HashMap<String, serde_json::Value>,
}

impl PyCustomDataPoint {
    pub fn from_point(point: &CustomDataPoint) -> Self {
        PyCustomDataPoint {
            value: point.value.to_f64().unwrap_or(0.0),
            time: point.time,
            end_time: point
                .end_time
                .map(|time| ns_to_naive(time.0))
                .unwrap_or_else(|| point.time.and_hms_opt(0, 0, 0).unwrap_or_default()),
            fields_inner: point.fields.clone(),
        }
    }
}

#[pymethods]
impl PyCustomDataPoint {
    /// Extra fields dict — `data.custom["VIX"].fields["open"]`.
    #[getter]
    fn fields(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        use pyo3::types::PyDict;
        let dict = PyDict::new(py);
        for (k, v) in &self.fields_inner {
            let py_val = json_value_to_py(py, v)?;
            dict.set_item(k, py_val)?;
        }
        Ok(dict.into())
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        {
            let borrowed = slf.borrow();
            if let Some(value) = borrowed.fields_inner.get(name) {
                return json_value_to_py(slf.py(), value);
            }
            if snake != name {
                if let Some(value) = borrowed.fields_inner.get(&snake) {
                    return json_value_to_py(slf.py(), value);
                }
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'CustomDataPoint' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!("CustomDataPoint(time={} value={})", self.time, self.value)
    }
}

/// Convert a `serde_json::Value` to a Python object.
fn json_value_to_py(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    match v {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => (*b).into_py_any(py),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py_any(py)
            } else if let Some(f) = n.as_f64() {
                f.into_py_any(py)
            } else {
                n.to_string().into_py_any(py)
            }
        }
        serde_json::Value::String(s) => s.as_str().into_py_any(py),
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
/// For multi-row sources (e.g. snapshot files with one row per symbol), use
/// `get_all(ticker)` to retrieve all points delivered for that ticker today.
#[pyclass(name = "CustomData")]
pub struct PyCustomData {
    /// ticker (uppercase) → all data points for this bar (last = most recent)
    points: HashMap<String, Vec<Py<PyCustomDataPoint>>>,
}

impl PyCustomData {
    pub fn empty() -> Self {
        PyCustomData {
            points: HashMap::new(),
        }
    }

    pub fn from_points(
        py: Python<'_>,
        data: &HashMap<String, Vec<CustomDataPoint>>,
    ) -> PyResult<Self> {
        let mut points = HashMap::with_capacity(data.len());
        for (ticker, rows) in data {
            let mut py_points = Vec::with_capacity(rows.len());
            for row in rows {
                py_points.push(Py::new(py, PyCustomDataPoint::from_point(row))?);
            }
            points.insert(ticker.to_uppercase(), py_points);
        }
        Ok(PyCustomData { points })
    }
}

#[pymethods]
impl PyCustomData {
    /// Returns the LAST (most recent) point for the ticker — LEAN-compatible single-point API.
    fn __getitem__(&self, py: Python<'_>, ticker: &str) -> PyResult<Option<Py<PyCustomDataPoint>>> {
        let key = ticker.to_uppercase();
        Ok(self
            .points
            .get(&key)
            .and_then(|v| v.last())
            .map(|p| p.clone_ref(py)))
    }

    /// Returns the LAST (most recent) point for the ticker — LEAN-compatible single-point API.
    fn get(&self, py: Python<'_>, ticker: &str) -> PyResult<Option<Py<PyCustomDataPoint>>> {
        self.__getitem__(py, ticker)
    }

    /// Returns ALL points for the ticker delivered today.
    ///
    /// Use this for snapshot/universe sources that push one row per symbol per day
    /// (e.g. `data.custom.get_all("snapshot")` → list of CustomDataPoint).
    fn get_all(&self, py: Python<'_>, ticker: &str) -> Vec<Py<PyCustomDataPoint>> {
        let key = ticker.to_uppercase();
        match self.points.get(&key) {
            Some(v) => v.iter().map(|p| p.clone_ref(py)).collect(),
            None => vec![],
        }
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
    /// The TradeBars container object (shared with py_slice).
    bars_cell: Py<PyTradeBars>,
    /// Per-symbol mutable quote bar cells, keyed by symbol SID.
    quote_bar_cells: HashMap<u64, Py<PyQuoteBar>>,
    /// The QuoteBars container object (shared with py_slice).
    quote_bars_cell: Py<PyQuoteBars>,
    /// The MarginInterestRates container object (shared with py_slice).
    margin_interest_rates_cell: Py<PyMarginInterestRates>,
    /// The PerpetualContexts container object (shared with py_slice).
    perpetual_contexts_cell: Py<PyPerpetualContexts>,
    /// The Ticks container object (shared with py_slice).
    ticks_cell: Py<PyTicks>,
    /// Mutable option chains cell — updated in-place each bar.
    option_chains_cell: Py<PyOptionChains>,
    /// Mutable custom data cell — updated once per trading day.
    custom_data_cell: Py<PyCustomData>,
    /// Mutable delistings cell — updated each day.
    pub delistings_cell: Py<PyDelistings>,
    /// Mutable symbol changed events cell — updated each day.
    pub symbol_changed_events_cell: Py<PySymbolChangedEvents>,
}

/// Reusable Python `Slice` for framework alpha models.
///
/// Python alpha `Update()` is called from the framework pipeline and only receives
/// a raw Rust `Slice`, not the runner's subscription-backed `SliceProxy`. This
/// cache keeps the same Python Slice surface while updating the backing
/// containers in place to avoid rebuilding the whole Python object graph on every
/// framework update.
pub struct FrameworkSliceProxy {
    py_slice: Py<PySlice>,
    bar_cells: HashMap<u64, Py<PyTradeBar>>,
    quote_bar_cells: HashMap<u64, Py<PyQuoteBar>>,
    active_bar_sids: Vec<u64>,
    active_quote_bar_sids: Vec<u64>,
    bars_cell: Py<PyTradeBars>,
    quote_bars_cell: Py<PyQuoteBars>,
    margin_interest_rates_cell: Py<PyMarginInterestRates>,
    perpetual_contexts_cell: Py<PyPerpetualContexts>,
    ticks_cell: Py<PyTicks>,
    custom_data_cell: Py<PyCustomData>,
    delistings_cell: Py<PyDelistings>,
    symbol_changed_events_cell: Py<PySymbolChangedEvents>,
}

impl FrameworkSliceProxy {
    pub fn new(py: Python<'_>) -> PyResult<Self> {
        let py_bars = Py::new(
            py,
            PyTradeBars {
                bars: HashMap::new(),
                ticker_to_sid: HashMap::new(),
            },
        )?;
        let py_quote_bars = Py::new(py, PyQuoteBars::empty())?;
        let py_margin_interest_rates = Py::new(
            py,
            PyMarginInterestRates {
                rates: HashMap::new(),
                ticker_to_sid: HashMap::new(),
            },
        )?;
        let py_perpetual_contexts = Py::new(
            py,
            PyPerpetualContexts {
                contexts: HashMap::new(),
                ticker_to_sid: HashMap::new(),
            },
        )?;
        let py_ticks = Py::new(py, PyTicks::empty())?;
        let py_custom = Py::new(py, PyCustomData::empty())?;
        let py_delistings = Py::new(py, PyDelistings::empty())?;
        let py_sce = Py::new(py, PySymbolChangedEvents::empty())?;
        let py_chains = Py::new(py, PyOptionChains::empty())?;
        let py_slice = Py::new(
            py,
            PySlice {
                bars_obj: py_bars.clone_ref(py),
                quote_bars_obj: py_quote_bars.clone_ref(py),
                margin_interest_rates_obj: py_margin_interest_rates.clone_ref(py),
                perpetual_contexts_obj: py_perpetual_contexts.clone_ref(py),
                ticks_obj: py_ticks.clone_ref(py),
                option_chains_obj: py_chains,
                custom_data_obj: py_custom.clone_ref(py),
                delistings_obj: py_delistings.clone_ref(py),
                symbol_changed_events_obj: py_sce.clone_ref(py),
                has_data: false,
            },
        )?;

        Ok(Self {
            py_slice,
            bar_cells: HashMap::new(),
            quote_bar_cells: HashMap::new(),
            active_bar_sids: Vec::new(),
            active_quote_bar_sids: Vec::new(),
            bars_cell: py_bars,
            quote_bars_cell: py_quote_bars,
            margin_interest_rates_cell: py_margin_interest_rates,
            perpetual_contexts_cell: py_perpetual_contexts,
            ticks_cell: py_ticks,
            custom_data_cell: py_custom,
            delistings_cell: py_delistings,
            symbol_changed_events_cell: py_sce,
        })
    }

    pub fn update(&mut self, py: Python<'_>, slice: &Slice) -> Py<PyAny> {
        self.update_bars(py, slice);
        self.update_quote_bars(py, &slice.quote_bars);
        self.update_margin_interest_rates(py, slice);
        self.update_perpetual_contexts(py, slice);
        self.update_ticks(py, &slice.ticks);
        self.update_custom_data(py, &slice.custom_data);
        self.update_delistings(py, slice);
        self.update_symbol_changed_events(py, slice);
        self.py_slice.borrow_mut(py).has_data = slice.has_data || !slice.custom_data.is_empty();
        self.py_slice.clone_ref(py).into_any()
    }

    fn update_quote_bar_membership(&mut self, py: Python<'_>, quote_bars: &HashMap<u64, QuoteBar>) {
        let mut qbars_obj = self.quote_bars_cell.borrow_mut(py);
        let previous_sids = std::mem::take(&mut self.active_quote_bar_sids);
        for sid in &previous_sids {
            if !quote_bars.contains_key(sid) {
                qbars_obj.bars.remove(sid);
            }
        }
        let current_len = qbars_obj.bars.len();
        qbars_obj
            .bars
            .reserve(quote_bars.len().saturating_sub(current_len));
        self.active_quote_bar_sids.reserve(quote_bars.len());
        for (&sid, qbar) in quote_bars {
            if qbars_obj
                .ticker_to_sid
                .get(qbar.symbol.value.as_str())
                .copied()
                != Some(sid)
            {
                qbars_obj
                    .ticker_to_sid
                    .insert(qbar.symbol.value.clone(), sid);
            }
            if qbars_obj
                .ticker_to_sid
                .get(qbar.symbol.permtick.as_str())
                .copied()
                != Some(sid)
            {
                qbars_obj
                    .ticker_to_sid
                    .insert(qbar.symbol.permtick.clone(), sid);
            }
            if !previous_sids.contains(&sid) {
                if let Some(cell) = self.quote_bar_cells.get(&sid) {
                    qbars_obj.bars.insert(sid, cell.clone_ref(py));
                }
            }
            self.active_quote_bar_sids.push(sid);
        }
    }

    fn update_bars_membership(&mut self, py: Python<'_>, bars: &HashMap<u64, TradeBar>) {
        let mut bars_obj = self.bars_cell.borrow_mut(py);
        let previous_sids = std::mem::take(&mut self.active_bar_sids);
        for sid in &previous_sids {
            if !bars.contains_key(sid) {
                bars_obj.bars.remove(sid);
            }
        }
        let current_len = bars_obj.bars.len();
        bars_obj
            .bars
            .reserve(bars.len().saturating_sub(current_len));
        self.active_bar_sids.reserve(bars.len());
        for (&sid, bar) in bars {
            if bars_obj
                .ticker_to_sid
                .get(bar.symbol.value.as_str())
                .copied()
                != Some(sid)
            {
                bars_obj.ticker_to_sid.insert(bar.symbol.value.clone(), sid);
            }
            if bars_obj
                .ticker_to_sid
                .get(bar.symbol.permtick.as_str())
                .copied()
                != Some(sid)
            {
                bars_obj
                    .ticker_to_sid
                    .insert(bar.symbol.permtick.clone(), sid);
            }
            if !previous_sids.contains(&sid) {
                if let Some(cell) = self.bar_cells.get(&sid) {
                    bars_obj.bars.insert(sid, cell.clone_ref(py));
                }
            }
            self.active_bar_sids.push(sid);
        }
    }

    fn update_bars(&mut self, py: Python<'_>, slice: &Slice) {
        self.bar_cells
            .reserve(slice.bars.len().saturating_sub(self.bar_cells.len()));
        for (&sid, bar) in &slice.bars {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.bar_cells.entry(sid) {
                match Py::new(py, PyTradeBar::from(bar)) {
                    Ok(py_bar) => {
                        slot.insert(py_bar);
                    }
                    Err(e) => {
                        tracing::warn!("FrameworkSliceProxy: TradeBar alloc error: {e}");
                        continue;
                    }
                }
            }
            if let Some(py_bar) = self.bar_cells.get(&sid) {
                let mut b = py_bar.borrow_mut(py);
                if b.symbol.inner.value != bar.symbol.value
                    || b.symbol.inner.permtick != bar.symbol.permtick
                {
                    b.symbol.inner = bar.symbol.clone();
                }
                b.open = bar.open.to_f64().unwrap_or(0.0);
                b.high = bar.high.to_f64().unwrap_or(0.0);
                b.low = bar.low.to_f64().unwrap_or(0.0);
                b.close = bar.close.to_f64().unwrap_or(0.0);
                b.volume = bar.volume.to_f64().unwrap_or(0.0);
                b.time = ns_to_naive(bar.time.0);
                b.end_time = ns_to_naive(bar.end_time.0);
            }
        }
        self.update_bars_membership(py, &slice.bars);
    }

    fn update_quote_bars(&mut self, py: Python<'_>, quote_bars: &HashMap<u64, QuoteBar>) {
        let to_f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);
        self.quote_bar_cells
            .reserve(quote_bars.len().saturating_sub(self.quote_bar_cells.len()));
        for (&sid, qbar) in quote_bars {
            if let std::collections::hash_map::Entry::Vacant(slot) = self.quote_bar_cells.entry(sid) {
                match Py::new(py, PyQuoteBar::from(qbar)) {
                    Ok(py_qbar) => {
                        slot.insert(py_qbar);
                    }
                    Err(e) => {
                        tracing::warn!("FrameworkSliceProxy: QuoteBar alloc error: {e}");
                        continue;
                    }
                }
            }
            if let Some(py_qbar) = self.quote_bar_cells.get(&sid) {
                let mut b = py_qbar.borrow_mut(py);
                if b.symbol.inner.value != qbar.symbol.value
                    || b.symbol.inner.permtick != qbar.symbol.permtick
                {
                    b.symbol.inner = qbar.symbol.clone();
                }
                b.bid_open = qbar.bid.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
                b.bid_high = qbar.bid.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
                b.bid_low = qbar.bid.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
                b.bid_close = qbar.bid.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
                b.ask_open = qbar.ask.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
                b.ask_high = qbar.ask.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
                b.ask_low = qbar.ask.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
                b.ask_close = qbar.ask.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
                b.bid_size = to_f(qbar.last_bid_size);
                b.ask_size = to_f(qbar.last_ask_size);
                b.time = ns_to_naive(qbar.time.0);
                b.end_time = ns_to_naive(qbar.end_time.0);
            }
        }
        self.update_quote_bar_membership(py, quote_bars);
    }

    fn update_margin_interest_rates(&mut self, py: Python<'_>, slice: &Slice) {
        let mut rates_obj = self.margin_interest_rates_cell.borrow_mut(py);
        rates_obj.rates.clear();
        rates_obj.rates.reserve(slice.margin_interest_rates.len());
        rates_obj
            .ticker_to_sid
            .reserve(slice.margin_interest_rates.len().saturating_mul(2));
        for (&sid, rate) in &slice.margin_interest_rates {
            rates_obj
                .ticker_to_sid
                .insert(rate.symbol.value.clone(), sid);
            rates_obj
                .ticker_to_sid
                .insert(rate.symbol.permtick.clone(), sid);
            if let Ok(py_rate) = Py::new(py, PyMarginInterestRate::from(rate)) {
                rates_obj.rates.insert(sid, py_rate);
            }
        }
    }

    fn update_perpetual_contexts(&mut self, py: Python<'_>, slice: &Slice) {
        let mut contexts_obj = self.perpetual_contexts_cell.borrow_mut(py);
        contexts_obj.contexts.clear();
        contexts_obj
            .contexts
            .reserve(slice.perpetual_contexts.len());
        contexts_obj
            .ticker_to_sid
            .reserve(slice.perpetual_contexts.len().saturating_mul(2));
        for (&sid, context) in &slice.perpetual_contexts {
            contexts_obj
                .ticker_to_sid
                .insert(context.symbol.value.clone(), sid);
            contexts_obj
                .ticker_to_sid
                .insert(context.symbol.permtick.clone(), sid);
            if let Ok(py_context) = Py::new(py, PyPerpetualContext::from(context)) {
                contexts_obj.contexts.insert(sid, py_context);
            }
        }
    }

    fn update_ticks(&mut self, py: Python<'_>, ticks: &HashMap<u64, Vec<Tick>>) {
        let mut ticks_obj = self.ticks_cell.borrow_mut(py);
        ticks_obj.ticks.clear();
        ticks_obj.ticker_to_sid.clear();
        ticks_obj.ticks.reserve(ticks.len());
        ticks_obj
            .ticker_to_sid
            .reserve(ticks.len().saturating_mul(2));

        for (&sid, tick_vec) in ticks {
            let Some(first) = tick_vec.first() else {
                continue;
            };
            ticks_obj
                .ticker_to_sid
                .insert(first.symbol.value.clone(), sid);
            ticks_obj
                .ticker_to_sid
                .insert(first.symbol.permtick.clone(), sid);
            let mut py_ticks = Vec::with_capacity(tick_vec.len());
            for tick in tick_vec {
                if let Ok(py_tick) = Py::new(py, PyTick::from(tick)) {
                    py_ticks.push(py_tick);
                }
            }
            ticks_obj.ticks.insert(sid, py_ticks);
        }
    }

    fn update_custom_data(&mut self, py: Python<'_>, data: &HashMap<String, Vec<CustomDataPoint>>) {
        let mut custom_obj = self.custom_data_cell.borrow_mut(py);
        custom_obj.points.clear();
        custom_obj.points.reserve(data.len());

        for (ticker, points) in data {
            if points.is_empty() {
                continue;
            }
            let mut py_points = Vec::with_capacity(points.len());
            for point in points {
                match Py::new(py, PyCustomDataPoint::from_point(point)) {
                    Ok(py_point) => py_points.push(py_point),
                    Err(e) => {
                        tracing::warn!("FrameworkSliceProxy: custom point alloc error: {e}");
                    }
                }
            }
            if !py_points.is_empty() {
                custom_obj.points.insert(ticker.to_uppercase(), py_points);
            }
        }
    }

    fn update_delistings(&mut self, py: Python<'_>, slice: &Slice) {
        let mut dl = self.delistings_cell.borrow_mut(py);
        dl.events.clear();
        dl.events.reserve(slice.delistings.len());
        for (&sid, d) in &slice.delistings {
            if let Ok(py_d) = Py::new(py, PyDelisting::from(d)) {
                dl.events.insert(sid, py_d);
            }
        }
    }

    fn update_symbol_changed_events(&mut self, py: Python<'_>, slice: &Slice) {
        let mut sce = self.symbol_changed_events_cell.borrow_mut(py);
        sce.events.clear();
        sce.events.reserve(slice.symbol_changed_events.len());
        for (&sid, ev) in &slice.symbol_changed_events {
            if let Ok(py_ev) = Py::new(py, PySymbolChangedEvent::from(ev)) {
                sce.events.insert(sid, py_ev);
            }
        }
    }
}

impl SliceProxy {
    /// Allocate one `PyTradeBar` per subscription.  One-time cost paid before
    /// the main loop; amortised over all trading days.
    pub fn new(py: Python<'_>, subscriptions: &[Arc<SubscriptionDataConfig>]) -> PyResult<Self> {
        let alias_capacity = subscriptions.len().saturating_mul(2);
        let mut bar_cells: HashMap<u64, Py<PyTradeBar>> =
            HashMap::with_capacity(subscriptions.len());
        let mut quote_bar_cells: HashMap<u64, Py<PyQuoteBar>> =
            HashMap::with_capacity(subscriptions.len());
        let mut ticker_to_sid: HashMap<String, u64> = HashMap::with_capacity(alias_capacity);
        let mut qb_ticker_to_sid: HashMap<String, u64> = HashMap::with_capacity(alias_capacity);
        let mut margin_ticker_to_sid: HashMap<String, u64> = HashMap::with_capacity(alias_capacity);
        let mut perpetual_context_ticker_to_sid: HashMap<String, u64> =
            HashMap::with_capacity(alias_capacity);

        for sub in subscriptions {
            let sid = sub.symbol.id.sid;
            let py_bar = Py::new(
                py,
                PyTradeBar {
                    open: 0.0,
                    high: 0.0,
                    low: 0.0,
                    close: 0.0,
                    volume: 0.0,
                    symbol: PySymbol {
                        inner: sub.symbol.clone(),
                    },
                    time: chrono::NaiveDateTime::default(),
                    end_time: chrono::NaiveDateTime::default(),
                },
            )?;
            ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            ticker_to_sid.insert(sub.symbol.permtick.clone(), sid);
            bar_cells.insert(sid, py_bar);

            let py_qbar = Py::new(
                py,
                PyQuoteBar {
                    bid_open: 0.0,
                    bid_high: 0.0,
                    bid_low: 0.0,
                    bid_close: 0.0,
                    ask_open: 0.0,
                    ask_high: 0.0,
                    ask_low: 0.0,
                    ask_close: 0.0,
                    bid_size: 0.0,
                    ask_size: 0.0,
                    symbol: PySymbol {
                        inner: sub.symbol.clone(),
                    },
                    time: chrono::NaiveDateTime::default(),
                    end_time: chrono::NaiveDateTime::default(),
                },
            )?;
            qb_ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            qb_ticker_to_sid.insert(sub.symbol.permtick.clone(), sid);
            margin_ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            margin_ticker_to_sid.insert(sub.symbol.permtick.clone(), sid);
            perpetual_context_ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            perpetual_context_ticker_to_sid.insert(sub.symbol.permtick.clone(), sid);
            quote_bar_cells.insert(sid, py_qbar);
        }

        let py_bars_obj = Py::new(
            py,
            PyTradeBars {
                bars: HashMap::new(),
                ticker_to_sid,
            },
        )?;
        let py_chains = Py::new(py, PyOptionChains::empty())?;
        let py_qbars_obj = Py::new(
            py,
            PyQuoteBars {
                bars: HashMap::new(),
                ticker_to_sid: qb_ticker_to_sid,
            },
        )?;
        let py_margin_interest_rates = Py::new(
            py,
            PyMarginInterestRates {
                rates: HashMap::new(),
                ticker_to_sid: margin_ticker_to_sid,
            },
        )?;
        let py_perpetual_contexts = Py::new(
            py,
            PyPerpetualContexts {
                contexts: HashMap::new(),
                ticker_to_sid: perpetual_context_ticker_to_sid,
            },
        )?;
        let py_ticks_obj = Py::new(py, PyTicks::empty())?;
        let py_custom = Py::new(py, PyCustomData::empty())?;
        let py_delistings = Py::new(py, PyDelistings::empty())?;
        let py_sce = Py::new(py, PySymbolChangedEvents::empty())?;
        let py_slice = Py::new(
            py,
            PySlice {
                bars_obj: py_bars_obj.clone_ref(py),
                quote_bars_obj: py_qbars_obj.clone_ref(py),
                margin_interest_rates_obj: py_margin_interest_rates.clone_ref(py),
                perpetual_contexts_obj: py_perpetual_contexts.clone_ref(py),
                ticks_obj: py_ticks_obj.clone_ref(py),
                option_chains_obj: py_chains.clone_ref(py),
                custom_data_obj: py_custom.clone_ref(py),
                delistings_obj: py_delistings.clone_ref(py),
                symbol_changed_events_obj: py_sce.clone_ref(py),
                has_data: false,
            },
        )?;

        Ok(SliceProxy {
            py_slice,
            bar_cells,
            bars_cell: py_bars_obj,
            quote_bar_cells,
            quote_bars_cell: py_qbars_obj,
            margin_interest_rates_cell: py_margin_interest_rates,
            perpetual_contexts_cell: py_perpetual_contexts,
            ticks_cell: py_ticks_obj,
            option_chains_cell: py_chains,
            custom_data_cell: py_custom,
            delistings_cell: py_delistings,
            symbol_changed_events_cell: py_sce,
        })
    }

    /// Add cells for a subscription created after the backtest loop started.
    ///
    /// C# LEAN's data feed can add universe subscriptions mid-stream. rlean
    /// keeps stable Python containers, so new symbols need cells registered
    /// before their later slices can expose `data.bars[symbol]`.
    pub fn add_subscription(
        &mut self,
        py: Python<'_>,
        sub: &Arc<SubscriptionDataConfig>,
    ) -> PyResult<()> {
        let sid = sub.symbol.id.sid;
        if self.bar_cells.contains_key(&sid) {
            return Ok(());
        }

        let py_bar = Py::new(
            py,
            PyTradeBar {
                open: 0.0,
                high: 0.0,
                low: 0.0,
                close: 0.0,
                volume: 0.0,
                symbol: PySymbol {
                    inner: sub.symbol.clone(),
                },
                time: chrono::NaiveDateTime::default(),
                end_time: chrono::NaiveDateTime::default(),
            },
        )?;
        self.bar_cells.insert(sid, py_bar);
        {
            let mut bars_obj = self.bars_cell.borrow_mut(py);
            bars_obj.ticker_to_sid.insert(sub.symbol.value.clone(), sid);
            bars_obj
                .ticker_to_sid
                .insert(sub.symbol.permtick.clone(), sid);
        }

        let py_qbar = Py::new(
            py,
            PyQuoteBar {
                bid_open: 0.0,
                bid_high: 0.0,
                bid_low: 0.0,
                bid_close: 0.0,
                ask_open: 0.0,
                ask_high: 0.0,
                ask_low: 0.0,
                ask_close: 0.0,
                bid_size: 0.0,
                ask_size: 0.0,
                symbol: PySymbol {
                    inner: sub.symbol.clone(),
                },
                time: chrono::NaiveDateTime::default(),
                end_time: chrono::NaiveDateTime::default(),
            },
        )?;
        self.quote_bar_cells.insert(sid, py_qbar);
        {
            let mut qbars_obj = self.quote_bars_cell.borrow_mut(py);
            qbars_obj
                .ticker_to_sid
                .insert(sub.symbol.value.clone(), sid);
            qbars_obj
                .ticker_to_sid
                .insert(sub.symbol.permtick.clone(), sid);
        }
        {
            let mut rates_obj = self.margin_interest_rates_cell.borrow_mut(py);
            rates_obj
                .ticker_to_sid
                .insert(sub.symbol.value.clone(), sid);
            rates_obj
                .ticker_to_sid
                .insert(sub.symbol.permtick.clone(), sid);
        }
        {
            let mut contexts_obj = self.perpetual_contexts_cell.borrow_mut(py);
            contexts_obj
                .ticker_to_sid
                .insert(sub.symbol.value.clone(), sid);
            contexts_obj
                .ticker_to_sid
                .insert(sub.symbol.permtick.clone(), sid);
        }

        Ok(())
    }

    /// Drop proxy cells and ticker aliases for symbols no longer present in
    /// the algorithm's active SubscriptionManager snapshot.
    pub fn retain_subscriptions(
        &mut self,
        py: Python<'_>,
        subscriptions: &[Arc<SubscriptionDataConfig>],
    ) {
        let active_sids: std::collections::HashSet<u64> =
            subscriptions.iter().map(|sub| sub.symbol.id.sid).collect();

        self.bar_cells.retain(|sid, _| active_sids.contains(sid));
        self.quote_bar_cells
            .retain(|sid, _| active_sids.contains(sid));

        {
            let mut bars_obj = self.bars_cell.borrow_mut(py);
            bars_obj.bars.retain(|sid, _| active_sids.contains(sid));
            bars_obj
                .ticker_to_sid
                .retain(|_, sid| active_sids.contains(sid));
        }

        {
            let mut qbars_obj = self.quote_bars_cell.borrow_mut(py);
            qbars_obj.bars.retain(|sid, _| active_sids.contains(sid));
            qbars_obj
                .ticker_to_sid
                .retain(|_, sid| active_sids.contains(sid));
        }
        {
            let mut rates_obj = self.margin_interest_rates_cell.borrow_mut(py);
            rates_obj.rates.retain(|sid, _| active_sids.contains(sid));
            rates_obj
                .ticker_to_sid
                .retain(|_, sid| active_sids.contains(sid));
        }
        {
            let mut contexts_obj = self.perpetual_contexts_cell.borrow_mut(py);
            contexts_obj
                .contexts
                .retain(|sid, _| active_sids.contains(sid));
            contexts_obj
                .ticker_to_sid
                .retain(|_, sid| active_sids.contains(sid));
        }
    }

    /// Write new bar values in-place.  Zero allocation; ~5 f64 writes + 2 string
    /// formats per symbol.  Must be called with the GIL held and no active Python
    /// borrows on the bar objects (guaranteed safe between `on_data` calls).
    pub fn update(&self, py: Python<'_>, slice: &Slice) {
        let mut active_sids = Vec::with_capacity(slice.bars.len());
        for (&sid, bar) in &slice.bars {
            if let Some(py_bar) = self.bar_cells.get(&sid) {
                let mut b = py_bar.borrow_mut(py);
                b.open = bar.open.to_f64().unwrap_or(0.0);
                b.high = bar.high.to_f64().unwrap_or(0.0);
                b.low = bar.low.to_f64().unwrap_or(0.0);
                b.close = bar.close.to_f64().unwrap_or(0.0);
                b.volume = bar.volume.to_f64().unwrap_or(0.0);
                b.time = ns_to_naive(bar.time.0);
                b.end_time = ns_to_naive(bar.end_time.0);
                active_sids.push(sid);
            }
        }
        {
            let mut bars_obj = self.bars_cell.borrow_mut(py);
            bars_obj.bars.clear();
            bars_obj.bars.reserve(slice.bars.len());
            for sid in active_sids {
                if let Some(cell) = self.bar_cells.get(&sid) {
                    bars_obj.bars.insert(sid, cell.clone_ref(py));
                }
            }
        }
        self.py_slice.borrow_mut(py).has_data = slice.has_data;

        // Update delistings for this bar.
        {
            let mut dl = self.delistings_cell.borrow_mut(py);
            dl.events.clear();
            dl.events.reserve(slice.delistings.len());
            for (&sid, d) in &slice.delistings {
                if let Ok(py_d) = Py::new(py, PyDelisting::from(d)) {
                    dl.events.insert(sid, py_d);
                }
            }
        }

        // Update symbol changed events for this bar.
        {
            let mut sce = self.symbol_changed_events_cell.borrow_mut(py);
            sce.events.clear();
            sce.events.reserve(slice.symbol_changed_events.len());
            for (&sid, ev) in &slice.symbol_changed_events {
                if let Ok(py_ev) = Py::new(py, PySymbolChangedEvent::from(ev)) {
                    sce.events.insert(sid, py_ev);
                }
            }
        }
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
                b.bid_open = qbar.bid.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
                b.bid_high = qbar.bid.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
                b.bid_low = qbar.bid.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
                b.bid_close = qbar.bid.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
                b.ask_open = qbar.ask.as_ref().map(|b| to_f(b.open)).unwrap_or(0.0);
                b.ask_high = qbar.ask.as_ref().map(|b| to_f(b.high)).unwrap_or(0.0);
                b.ask_low = qbar.ask.as_ref().map(|b| to_f(b.low)).unwrap_or(0.0);
                b.ask_close = qbar.ask.as_ref().map(|b| to_f(b.close)).unwrap_or(0.0);
                b.bid_size = to_f(qbar.last_bid_size);
                b.ask_size = to_f(qbar.last_ask_size);
                b.time = ns_to_naive(qbar.time.0);
                b.end_time = ns_to_naive(qbar.end_time.0);
            }
        }

        // Update the QuoteBars container to only expose SIDs with data this minute.
        {
            let mut qbars_obj = self.quote_bars_cell.borrow_mut(py);
            qbars_obj.bars.clear();
            qbars_obj.bars.reserve(quote_bars.len());
            for &sid in quote_bars.keys() {
                if let Some(cell) = self.quote_bar_cells.get(&sid) {
                    qbars_obj.bars.insert(sid, cell.clone_ref(py));
                }
            }
        }
    }

    /// Replace the `data.margin_interest_rates` container for this slice.
    pub fn update_margin_interest_rates(&self, py: Python<'_>, slice: &Slice) {
        let mut rates_obj = self.margin_interest_rates_cell.borrow_mut(py);
        rates_obj.rates.clear();
        rates_obj.rates.reserve(slice.margin_interest_rates.len());
        for (&sid, rate) in &slice.margin_interest_rates {
            if let Ok(py_rate) = Py::new(py, PyMarginInterestRate::from(rate)) {
                rates_obj.rates.insert(sid, py_rate);
            }
        }
    }

    /// Replace the `data.perpetual_contexts` container for this slice.
    pub fn update_perpetual_contexts(&self, py: Python<'_>, slice: &Slice) {
        let mut contexts_obj = self.perpetual_contexts_cell.borrow_mut(py);
        contexts_obj.contexts.clear();
        contexts_obj
            .contexts
            .reserve(slice.perpetual_contexts.len());
        for (&sid, context) in &slice.perpetual_contexts {
            if let Ok(py_context) = Py::new(py, PyPerpetualContext::from(context)) {
                contexts_obj.contexts.insert(sid, py_context);
            }
        }
    }

    /// Replace the `data.ticks` container for this slice.
    pub fn update_ticks(&self, py: Python<'_>, ticks: &HashMap<u64, Vec<Tick>>) {
        let mut ticks_obj = self.ticks_cell.borrow_mut(py);
        ticks_obj.ticks.clear();
        ticks_obj.ticker_to_sid.clear();
        ticks_obj.ticks.reserve(ticks.len());
        ticks_obj
            .ticker_to_sid
            .reserve(ticks.len().saturating_mul(2));

        for (&sid, tick_vec) in ticks {
            if tick_vec.is_empty() {
                continue;
            }

            if let Some(first) = tick_vec.first() {
                ticks_obj
                    .ticker_to_sid
                    .insert(first.symbol.value.clone(), sid);
                ticks_obj
                    .ticker_to_sid
                    .insert(first.symbol.permtick.clone(), sid);
            }

            let mut py_ticks = Vec::with_capacity(tick_vec.len());
            for tick in tick_vec {
                if let Ok(py_tick) = Py::new(py, PyTick::from(tick)) {
                    py_ticks.push(py_tick);
                }
            }
            if !py_ticks.is_empty() {
                ticks_obj.ticks.insert(sid, py_ticks);
            }
        }
    }

    /// Write the option chains for this bar in-place.
    /// Called once per trading day before `on_data` when option subscriptions exist.
    pub fn update_option_chains(&self, py: Python<'_>, chains: &[(&str, &OptionChain)]) {
        let mut chains_obj = self.option_chains_cell.borrow_mut(py);
        let active_keys: std::collections::HashSet<&str> =
            chains.iter().map(|(permtick, _)| *permtick).collect();
        chains_obj.retain_key_refs(&active_keys);
        for (permtick, chain) in chains {
            chains_obj.set_or_update_ref(py, permtick, chain).ok();
        }
    }

    /// Write custom data points for this bar in-place.
    ///
    /// Replaces the `data.custom` dict with ALL points for each ticker.
    /// Called once per trading day (or once per minute in minute-mode) before `on_data`.
    ///
    /// `data`: ticker (any case) → list of `CustomDataPoint`s for this date.
    /// All points are stored; `get()` returns the last, `get_all()` returns the full list.
    pub fn update_custom_data(&self, py: Python<'_>, data: &HashMap<String, Vec<CustomDataPoint>>) {
        let mut custom_obj = self.custom_data_cell.borrow_mut(py);
        custom_obj.points.clear();
        custom_obj.points.reserve(data.len());

        for (ticker, points) in data {
            if points.is_empty() {
                continue;
            }
            let mut py_points = Vec::with_capacity(points.len());
            for pt in points {
                match Py::new(py, PyCustomDataPoint::from_point(pt)) {
                    Ok(p) => py_points.push(p),
                    Err(_) => continue,
                }
            }
            custom_obj.points.insert(ticker.to_uppercase(), py_points);
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::py_qc_algorithm::pascal_to_snake;
    use lean_core::{Market, Resolution, Symbol};
    use lean_data::TradeBarData;
    use rust_decimal_macros::dec;

    fn make_trade_bar() -> PyTradeBar {
        PyTradeBar {
            open: 100.0,
            high: 105.0,
            low: 99.0,
            close: 102.0,
            volume: 1_000_000.0,
            symbol: PySymbol {
                inner: Symbol::create_equity("SPY", &Market::usa()),
            },
            time: chrono::NaiveDateTime::default(),
            end_time: chrono::NaiveDateTime::default(),
        }
    }

    /// C# LEAN TradeBar.Value == Close (BaseData convention).
    #[test]
    fn tradebar_value_equals_close() {
        let bar = make_trade_bar();
        assert!(
            (bar.value() - bar.close).abs() < 1e-9,
            "bar.value must equal bar.close"
        );
    }

    #[test]
    fn slice_proxy_retain_subscriptions_removes_stale_cells_and_aliases() {
        crate::test_python::init();
        Python::attach(|py| {
            let market = Market::usa();
            let spy = Symbol::create_equity("SPY", &market);
            let qqq = Symbol::create_equity("QQQ", &market);
            let spy_sub = Arc::new(SubscriptionDataConfig::new_equity(spy, Resolution::Minute));
            let qqq_sub = Arc::new(SubscriptionDataConfig::new_equity(qqq, Resolution::Minute));
            let mut proxy =
                SliceProxy::new(py, &[spy_sub.clone(), qqq_sub.clone()]).expect("proxy");

            proxy.retain_subscriptions(py, std::slice::from_ref(&spy_sub));

            assert!(proxy.bar_cells.contains_key(&spy_sub.symbol.id.sid));
            assert!(!proxy.bar_cells.contains_key(&qqq_sub.symbol.id.sid));
            assert!(proxy.quote_bar_cells.contains_key(&spy_sub.symbol.id.sid));
            assert!(!proxy.quote_bar_cells.contains_key(&qqq_sub.symbol.id.sid));

            let bars = proxy.bars_cell.borrow(py);
            assert_eq!(bars.ticker_to_sid.get("SPY"), Some(&spy_sub.symbol.id.sid));
            assert!(!bars.ticker_to_sid.contains_key("QQQ"));
            drop(bars);

            let quote_bars = proxy.quote_bars_cell.borrow(py);
            assert_eq!(
                quote_bars.ticker_to_sid.get("SPY"),
                Some(&spy_sub.symbol.id.sid)
            );
            assert!(!quote_bars.ticker_to_sid.contains_key("QQQ"));
        });
    }

    #[test]
    fn slice_from_slice_preserves_custom_data() {
        crate::test_python::init();
        Python::attach(|py| {
            let date = chrono::NaiveDate::from_ymd_opt(2025, 9, 3).unwrap();
            let mut slice = Slice::new(lean_core::DateTime::from(
                date.and_hms_opt(13, 46, 0).unwrap(),
            ));
            slice.custom_data.insert(
                "sweeps".to_string(),
                vec![CustomDataPoint {
                    time: date,
                    end_time: None,
                    value: rust_decimal::Decimal::ONE,
                    fields: HashMap::from([("usymbol".to_string(), serde_json::json!("ULCC"))]),
                }],
            );

            let py_slice = PySlice::from_slice(py, &slice).unwrap();
            let custom = py_slice.custom_data_obj.borrow(py);
            let points = custom.points.get("SWEEPS").unwrap();
            let point = points[0].borrow(py);

            assert!(py_slice.has_data);
            assert_eq!(points.len(), 1);
            assert_eq!(point.fields_inner.get("usymbol").unwrap(), "ULCC");
        });
    }

    #[test]
    fn framework_slice_proxy_updates_bars_and_custom_data() {
        crate::test_python::init();
        Python::attach(|py| {
            let market = Market::usa();
            let spy = Symbol::create_equity("SPY", &market);
            let qqq = Symbol::create_equity("QQQ", &market);
            let date = chrono::NaiveDate::from_ymd_opt(2025, 9, 3).unwrap();
            let time = lean_core::DateTime::from(date.and_hms_opt(13, 46, 0).unwrap());
            let period = lean_core::TimeSpan::from_secs(60);

            let mut first = Slice::new(time);
            first.add_bar(TradeBar::new(
                spy.clone(),
                time,
                period,
                TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100.50), dec!(1000)),
            ));
            first.custom_data.insert(
                "sweeps".to_string(),
                vec![CustomDataPoint {
                    time: date,
                    end_time: Some(time),
                    value: dec!(1),
                    fields: HashMap::from([("usymbol".to_string(), serde_json::json!("SPY"))]),
                }],
            );

            let mut proxy = FrameworkSliceProxy::new(py).expect("framework proxy");
            let _ = proxy.update(py, &first);
            {
                let bars = proxy.bars_cell.borrow(py);
                assert_eq!(bars.bars.len(), 1);
                let bar = bars.bars.get(&spy.id.sid).unwrap().borrow(py);
                assert_eq!(bar.close, 100.5);
                assert_eq!(bars.ticker_to_sid.get("SPY"), Some(&spy.id.sid));
            }
            {
                let custom = proxy.custom_data_cell.borrow(py);
                let points = custom.points.get("SWEEPS").unwrap();
                assert_eq!(points.len(), 1);
                assert_eq!(
                    points[0].borrow(py).fields_inner.get("usymbol").unwrap(),
                    "SPY"
                );
            }

            let mut second = Slice::new(time + period);
            second.add_bar(TradeBar::new(
                qqq.clone(),
                time + period,
                period,
                TradeBarData::new(dec!(200), dec!(202), dec!(198), dec!(201), dec!(2000)),
            ));
            let _ = proxy.update(py, &second);
            {
                let bars = proxy.bars_cell.borrow(py);
                assert_eq!(bars.bars.len(), 1);
                assert!(!bars.bars.contains_key(&spy.id.sid));
                let bar = bars.bars.get(&qqq.id.sid).unwrap().borrow(py);
                assert_eq!(bar.close, 201.0);
                assert_eq!(bars.ticker_to_sid.get("QQQ"), Some(&qqq.id.sid));
            }
            assert!(proxy.custom_data_cell.borrow(py).points.is_empty());
        });
    }

    /// All TradeBar PascalCase names must convert to valid snake_case properties
    /// so __getattr__ forwarding will find them at runtime.
    #[test]
    fn tradebar_pascal_names_convert_to_snake() {
        for (pascal, snake) in &[
            ("Close", "close"),
            ("Open", "open"),
            ("High", "high"),
            ("Low", "low"),
            ("Volume", "volume"),
            ("Symbol", "symbol"),
            ("Time", "time"),
            ("EndTime", "end_time"),
            ("Value", "value"),
        ] {
            assert_eq!(
                pascal_to_snake(pascal),
                *snake,
                "PascalCase '{}' should map to snake_case '{}'",
                pascal,
                snake
            );
        }
    }

    /// All OrderEvent PascalCase names must convert to valid snake_case.
    #[test]
    fn order_event_pascal_names_convert_to_snake() {
        for (pascal, snake) in &[
            ("FillPrice", "fill_price"),
            ("FillQuantity", "fill_quantity"),
            ("AbsoluteFillQuantity", "absolute_fill_quantity"),
            ("OrderId", "order_id"),
            ("Symbol", "symbol"),
            ("UtcTime", "utc_time"),
            ("Status", "status"),
            ("Direction", "direction"),
            ("Message", "message"),
            ("IsAssignment", "is_assignment"),
            ("IsFill", "is_fill"),
            ("OrderFee", "order_fee"),
            ("FillPriceCurrency", "fill_price_currency"),
        ] {
            assert_eq!(
                pascal_to_snake(pascal),
                *snake,
                "PascalCase '{}' should map to snake_case '{}'",
                pascal,
                snake
            );
        }
    }

    /// All OptionContract PascalCase names must convert to valid snake_case.
    #[test]
    fn option_contract_pascal_names_convert_to_snake() {
        for (pascal, snake) in &[
            ("Strike", "strike"),
            ("Expiry", "expiry"),
            ("Right", "right"),
            ("Style", "style"),
            ("BidPrice", "bid_price"),
            ("AskPrice", "ask_price"),
            ("LastPrice", "last_price"),
            ("ImpliedVolatility", "implied_volatility"),
            ("OpenInterest", "open_interest"),
            ("Greeks", "greeks"),
            ("Symbol", "symbol"),
            ("UnderlyingLastPrice", "underlying_last_price"),
            ("TheoreticalPrice", "theoretical_price"),
        ] {
            assert_eq!(
                pascal_to_snake(pascal),
                *snake,
                "PascalCase '{}' should map to snake_case '{}'",
                pascal,
                snake
            );
        }
    }

    /// Symbol.Value must map correctly.
    #[test]
    fn symbol_pascal_names_convert_to_snake() {
        assert_eq!(pascal_to_snake("Value"), "value");
        assert_eq!(pascal_to_snake("Ticker"), "ticker");
    }
}
