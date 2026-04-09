use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use pyo3::prelude::*;
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{Market, Resolution};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal_macros::dec;
use crate::charting::ChartCollection;
use crate::py_portfolio::PyPortfolio;
use crate::py_types::{PyResolution, PySecurity, PySymbol};

fn f2d(f: f64) -> Decimal {
    Decimal::from_f64(f).unwrap_or_default()
}

/// The base algorithm class that Python strategies inherit from.
///
/// ```python
/// from AlgorithmImports import *
///
/// class MyStrategy(QCAlgorithm):
///     def initialize(self):
///         self.set_start_date(2020, 1, 1)
///         self.set_end_date(2023, 12, 31)
///         self.set_cash(100_000)
///         self.spy = self.add_equity("SPY", Resolution.Daily).symbol
///         self.fast = SimpleMovingAverage(50)
///         self.slow = SimpleMovingAverage(200)
///
///     def on_data(self, data):
///         bar = data.bars.get(self.spy)
///         if bar is None: return
///         self.fast.update(self.time, bar.close)
///         self.slow.update(self.time, bar.close)
///         if not self.fast.is_ready or not self.slow.is_ready: return
///         if self.fast.current.value > self.slow.current.value and not self.portfolio[self.spy].invested:
///             self.set_holdings(self.spy, 1.0)
///         elif self.fast.current.value < self.slow.current.value and self.portfolio[self.spy].invested:
///             self.liquidate()
/// ```
#[pyclass(name = "QCAlgorithm", subclass)]
pub struct PyQcAlgorithm {
    pub inner: Arc<Mutex<QcAlgorithm>>,
    /// ticker → Symbol cache built as subscriptions are added
    pub symbols: HashMap<String, lean_core::Symbol>,
    /// Shared chart collection — plotted from Python via self.plot(...)
    pub charts: Arc<Mutex<ChartCollection>>,
}

impl PyQcAlgorithm {
    pub fn inner_arc(&self) -> Arc<Mutex<QcAlgorithm>> {
        self.inner.clone()
    }
    pub fn charts_arc(&self) -> Arc<Mutex<ChartCollection>> {
        self.charts.clone()
    }
}

#[pymethods]
impl PyQcAlgorithm {
    #[new]
    pub fn new() -> Self {
        PyQcAlgorithm {
            inner: Arc::new(Mutex::new(QcAlgorithm::new("PythonStrategy", dec!(100_000)))),
            symbols: HashMap::new(),
            charts: Arc::new(Mutex::new(ChartCollection::new())),
        }
    }

    // ─── Configuration ────────────────────────────────────────────────────────

    fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        self.inner.lock().unwrap().set_start_date(year, month, day);
    }

    fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        self.inner.lock().unwrap().set_end_date(year, month, day);
    }

    fn set_cash(&mut self, amount: f64) {
        self.inner.lock().unwrap().set_cash(f2d(amount));
    }

    /// Add (or subtract) cash directly — used to credit option premium
    /// or simulate assignment P&L adjustments.
    fn add_cash(&mut self, amount: f64) {
        let portfolio = self.inner.lock().unwrap().portfolio.clone();
        let delta = f2d(amount);
        *portfolio.cash.write() += delta;
    }

    fn set_name(&mut self, name: &str) {
        self.inner.lock().unwrap().name = name.to_string();
    }

    /// Set the warm-up period.
    ///
    /// If `bars_or_days` > 365 it is treated as a bar count; otherwise as a
    /// number of calendar days (which is consistent with C# LEAN's overloads).
    ///
    /// Examples (Python):
    ///   self.set_warm_up(200)   # 200 bars
    ///   self.set_warm_up(30)    # 30 days
    fn set_warm_up(&mut self, bars_or_days: i64) -> PyResult<()> {
        if bars_or_days > 365 {
            // treat as bar count
            self.inner.lock().unwrap().set_warm_up_bars(bars_or_days as usize);
        } else {
            // treat as days
            use lean_core::TimeSpan;
            let nanos = bars_or_days * 86_400 * 1_000_000_000i64;
            self.inner.lock().unwrap().set_warm_up(TimeSpan::from_nanos(nanos));
        }
        Ok(())
    }

    // ─── Universe ─────────────────────────────────────────────────────────────

    fn add_equity(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let sym = self.inner.lock().unwrap().add_equity(ticker, res);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        PySecurity { inner: PySymbol { inner: sym } }
    }

    fn add_forex(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let sym = self.inner.lock().unwrap().add_forex(ticker, res);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        PySecurity { inner: PySymbol { inner: sym } }
    }

    fn add_crypto(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let market = Market::usa(); // default; crypto can override
        let sym = self.inner.lock().unwrap().add_crypto(ticker, &market, res);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        PySecurity { inner: PySymbol { inner: sym } }
    }

    // ─── Ordering ─────────────────────────────────────────────────────────────

    /// Place a market order. symbol can be a PySymbol or a ticker string.
    fn market_order(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        self.inner.lock().unwrap().market_order(&sym, f2d(quantity));
        Ok(())
    }

    /// Place a limit order.
    fn limit_order(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64, limit_price: f64) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        self.inner.lock().unwrap().limit_order(&sym, f2d(quantity), f2d(limit_price));
        Ok(())
    }

    /// Place a stop-market order.
    fn stop_market_order(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64, stop_price: f64) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        self.inner.lock().unwrap().stop_market_order(&sym, f2d(quantity), f2d(stop_price));
        Ok(())
    }

    /// Target a portfolio weight (0.0 to 1.0). Automatically computes the delta order.
    fn set_holdings(&mut self, symbol: &Bound<'_, PyAny>, target: f64) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        self.inner.lock().unwrap().set_holdings(&sym, f2d(target));
        Ok(())
    }

    /// Liquidate a symbol (or all positions if symbol is None).
    #[pyo3(signature = (symbol=None))]
    fn liquidate(&mut self, symbol: Option<&Bound<'_, PyAny>>) -> PyResult<()> {
        match symbol {
            Some(s) => {
                let sym = self.resolve_symbol(s)?;
                self.inner.lock().unwrap().liquidate(Some(&sym));
            }
            None => {
                self.inner.lock().unwrap().liquidate(None);
            }
        }
        Ok(())
    }

    // ─── Options ──────────────────────────────────────────────────────────────

    /// Subscribe to an option chain for an underlying equity.
    /// Returns the canonical option ticker string (e.g. `?SPY`).
    #[pyo3(signature = (ticker, resolution=None))]
    fn add_option(&mut self, ticker: &str, resolution: Option<&str>) -> PyResult<String> {
        use lean_core::Resolution;
        let res = match resolution {
            Some("Daily") | Some("daily") => Resolution::Daily,
            Some("Hour") | Some("hour") => Resolution::Hour,
            Some("Minute") | Some("minute") => Resolution::Minute,
            _ => Resolution::Daily,
        };
        let canonical = self.inner.lock().unwrap().add_option(ticker, res);
        Ok(canonical.permtick.clone())
    }

    /// Get the current option chain for a canonical ticker (e.g. `?SPY`).
    /// Returns None if no chain data is available yet.
    fn get_option_chain(&self, canonical_ticker: &str) -> Option<crate::py_options::PyOptionChain> {
        self.inner.lock().unwrap()
            .get_option_chain(canonical_ticker)
            .map(|chain| crate::py_options::PyOptionChain { inner: chain })
    }

    /// Sell to open: write an option contract (collect premium).
    fn sell_to_open(&mut self, contract: &crate::py_options::PyOptionContract, quantity: f64, premium: f64) -> PyResult<i64> {
        use rust_decimal::prelude::FromPrimitive;
        let qty = Decimal::from_f64(quantity).unwrap_or(Decimal::ONE);
        let prem = f2d(premium);
        Ok(self.inner.lock().unwrap().sell_to_open(contract.inner.symbol.clone(), qty, prem))
    }

    /// Buy to open: long an option contract (pay premium).
    fn buy_to_open(&mut self, contract: &crate::py_options::PyOptionContract, quantity: f64, premium: f64) -> PyResult<i64> {
        use rust_decimal::prelude::FromPrimitive;
        let qty = Decimal::from_f64(quantity).unwrap_or(Decimal::ONE);
        let prem = f2d(premium);
        Ok(self.inner.lock().unwrap().buy_to_open(contract.inner.symbol.clone(), qty, prem))
    }

    /// Buy to close: cover a short option position.
    fn buy_to_close(&mut self, contract: &crate::py_options::PyOptionContract, quantity: f64, premium: f64) -> PyResult<i64> {
        use rust_decimal::prelude::FromPrimitive;
        let qty = Decimal::from_f64(quantity).unwrap_or(Decimal::ONE);
        let prem = f2d(premium);
        Ok(self.inner.lock().unwrap().buy_to_close(contract.inner.symbol.clone(), qty, prem))
    }

    /// Sell to close: exit a long option position.
    fn sell_to_close(&mut self, contract: &crate::py_options::PyOptionContract, quantity: f64, premium: f64) -> PyResult<i64> {
        use rust_decimal::prelude::FromPrimitive;
        let qty = Decimal::from_f64(quantity).unwrap_or(Decimal::ONE);
        let prem = f2d(premium);
        Ok(self.inner.lock().unwrap().sell_to_close(contract.inner.symbol.clone(), qty, prem))
    }

    /// Get all open option positions as a list of dicts.
    fn get_option_positions(&self) -> Vec<PyObject> {
        Python::with_gil(|py| {
            let inner = self.inner.lock().unwrap();
            inner.option_positions.values().map(|pos| {
                use rust_decimal::prelude::ToPrimitive;
                let dict = pyo3::types::PyDict::new(py);
                dict.set_item("ticker", pos.symbol.permtick.clone()).ok();
                dict.set_item("strike", pos.strike.to_f64().unwrap_or(0.0)).ok();
                dict.set_item("expiry", pos.expiry.to_string()).ok();
                dict.set_item("right", format!("{:?}", pos.right)).ok();
                dict.set_item("quantity", pos.quantity.to_f64().unwrap_or(0.0)).ok();
                dict.set_item("entry_price", pos.entry_price.to_f64().unwrap_or(0.0)).ok();
                dict.into_py(py)
            }).collect()
        })
    }

    // ─── Portfolio ────────────────────────────────────────────────────────────

    #[getter]
    fn portfolio(&self) -> PyPortfolio {
        let inner = self.inner.lock().unwrap();
        PyPortfolio { inner: inner.portfolio.clone() }
    }

    #[getter]
    fn cash(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.inner.lock().unwrap().cash().to_f64().unwrap_or(0.0)
    }

    #[getter]
    fn portfolio_value(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.inner.lock().unwrap().portfolio_value().to_f64().unwrap_or(0.0)
    }

    fn is_invested(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        let sym = self.resolve_symbol(symbol)?;
        Ok(self.inner.lock().unwrap().is_invested(&sym))
    }

    // ─── Time ────────────────────────────────────────────────────────────────

    /// Current algorithm time as a Python datetime object (matches LEAN's `self.time`).
    #[getter]
    fn time(&self) -> PyResult<PyObject> {
        let ns = self.inner.lock().unwrap().time.0;
        ns_to_py_datetime(ns)
    }

    /// Current UTC time as a Python datetime object.
    #[getter]
    fn utc_time(&self) -> PyResult<PyObject> {
        let ns = self.inner.lock().unwrap().utc_time.0;
        ns_to_py_datetime(ns)
    }

    /// Current algorithm time as an ISO string — kept for backwards compatibility.
    fn time_str(&self) -> String {
        let dt = self.inner.lock().unwrap().time;
        lean_datetime_to_iso(dt.0)
    }

    /// True during the warm-up period.
    #[getter]
    fn is_warming_up(&self) -> bool {
        self.inner.lock().unwrap().is_warming_up
    }

    // ─── Logging ─────────────────────────────────────────────────────────────

    fn log(&self, message: &str) {
        self.inner.lock().unwrap().log_message(message);
    }

    fn debug(&self, message: &str) {
        self.inner.lock().unwrap().debug(message);
    }

    // ─── Charting ─────────────────────────────────────────────────────────────

    /// Plot a value on a named chart/series using the current algorithm time.
    /// Usage: self.plot("My Chart", "RSI", rsi_value)
    fn plot(&self, chart: &str, series: &str, value: f64) -> PyResult<()> {
        let time_str = {
            let dt = self.inner.lock().unwrap().time;
            lean_datetime_to_date(dt.0)
        };
        if let Ok(mut c) = self.charts.lock() {
            c.plot(chart, series, &time_str, value);
        }
        Ok(())
    }

    /// Ensure a named chart exists in the collection (optional — plot() creates it automatically).
    fn add_chart(&self, name: &str) -> PyResult<()> {
        if let Ok(mut c) = self.charts.lock() {
            c.get_or_create(name);
        }
        Ok(())
    }

    // ─── Lifecycle hooks (overridden by Python subclasses) ────────────────────

    fn initialize(&mut self) {}
    fn on_data(&mut self, _data: PyObject) {}
    fn on_order_event(&mut self, _event: PyObject) {}
    fn on_end_of_algorithm(&mut self) {}
    fn on_warmup_finished(&mut self) {}

    fn __repr__(&self) -> String {
        let inner = self.inner.lock().unwrap();
        format!("QCAlgorithm(name='{}', value={:.2})", inner.name, inner.portfolio_value())
    }
}

fn ns_to_py_datetime(ns: i64) -> PyResult<PyObject> {
    Python::with_gil(|py| {
        let secs = ns / 1_000_000_000;
        let micros = (ns % 1_000_000_000) / 1_000;
        let timestamp = secs as f64 + micros as f64 / 1_000_000.0;
        let datetime = py.import("datetime")?
            .getattr("datetime")?
            .call_method1("utcfromtimestamp", (timestamp,))?;
        Ok(datetime.into())
    })
}

fn lean_datetime_to_iso(ns: i64) -> String {
    use chrono::{DateTime as ChronoDateTime, Utc};
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub)
        .unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Format a nanosecond timestamp as "YYYY-MM-DD" for chart points.
fn lean_datetime_to_date(ns: i64) -> String {
    use chrono::{DateTime as ChronoDateTime, Utc};
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub)
        .unwrap_or_default();
    dt.format("%Y-%m-%d").to_string()
}

impl PyQcAlgorithm {
    fn resolve_symbol(&self, arg: &Bound<'_, PyAny>) -> PyResult<lean_core::Symbol> {
        if let Ok(sym) = arg.downcast::<PySymbol>() {
            return Ok(sym.get().inner.clone());
        }
        // Accept Security objects directly (mirrors LEAN's set_holdings(security, ...) API)
        if let Ok(sec) = arg.downcast::<PySecurity>() {
            return Ok(sec.get().inner.inner.clone());
        }
        if let Ok(ticker) = arg.extract::<String>() {
            let upper = ticker.to_uppercase();
            if let Some(sym) = self.symbols.get(&upper) {
                return Ok(sym.clone());
            }
            // Fall back to creating a new US equity symbol
            return Ok(lean_core::Symbol::create_equity(&ticker, &Market::usa()));
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or ticker string"
        ))
    }
}
