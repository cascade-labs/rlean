use crate::charting::ChartCollection;
use crate::py_framework::{
    try_take_alpha, try_take_exec, try_take_pcm, try_take_risk, FrameworkState,
};
use crate::py_indicators::{PyEma, PyMomp, PyRsi, PySma, PyStd};
use crate::py_portfolio::PyPortfolio;
use crate::py_types::{PyAlgorithmSettings, PyResolution, PySecurity, PySecurityManager, PySymbol};
use crate::py_universe::{PyDateRules, PyScheduledUniverse, PyTimeRules, PyUniverseSettings};
use chrono::{Datelike, Timelike};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{Market, Resolution, SymbolOptionsExt};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Registry of auto-updating indicators keyed by symbol SID.
/// Each entry maps a SID to a Python indicator object that will be updated
/// with every new bar for that symbol (before `on_data` / `OnData` is called).
pub struct IndicatorRegistry {
    /// (sid, indicator_python_object) — updated via `update_bar(bar)` each day.
    pub entries: Vec<(u64, Py<PyAny>)>,
}

impl IndicatorRegistry {
    pub fn new() -> Self {
        IndicatorRegistry {
            entries: Vec::new(),
        }
    }
}

impl Default for IndicatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

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
///         self.spy = self.add_equity("SPY", Resolution.DAILY).symbol
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
    /// Algorithm Framework models (alpha, PCM, execution, risk).
    /// Shared with PyAlgorithmAdapter so the runner can execute the pipeline.
    pub framework: Arc<Mutex<FrameworkState>>,
    /// Registry of indicators to auto-update each bar.
    /// Shared with PyAlgorithmAdapter for pre-OnData updates.
    pub indicators: Arc<Mutex<IndicatorRegistry>>,
    /// LEAN universe settings shared between Python and the runner.
    pub universe_settings: PyUniverseSettings,
    /// Registered scheduled/user-defined universes.
    pub universes: Arc<Mutex<Vec<Py<PyScheduledUniverse>>>>,
}

impl PyQcAlgorithm {
    pub fn inner_arc(&self) -> Arc<Mutex<QcAlgorithm>> {
        self.inner.clone()
    }
    pub fn charts_arc(&self) -> Arc<Mutex<ChartCollection>> {
        self.charts.clone()
    }
    pub fn framework_arc(&self) -> Arc<Mutex<FrameworkState>> {
        self.framework.clone()
    }
    pub fn indicators_arc(&self) -> Arc<Mutex<IndicatorRegistry>> {
        self.indicators.clone()
    }
    pub fn universes_arc(&self) -> Arc<Mutex<Vec<Py<PyScheduledUniverse>>>> {
        self.universes.clone()
    }
}

fn py_properties_to_map(
    properties: Option<&Bound<'_, PyAny>>,
) -> PyResult<HashMap<String, String>> {
    let Some(properties) = properties else {
        return Ok(HashMap::new());
    };
    if properties.is_none() {
        return Ok(HashMap::new());
    }
    if let Ok(map) = properties.extract::<HashMap<String, String>>() {
        return Ok(map);
    }
    let dict = properties.cast::<PyDict>()?;
    let mut out = HashMap::new();
    for (key, value) in dict.iter() {
        let key = key.extract::<String>()?;
        let value = if let Ok(s) = value.extract::<String>() {
            s
        } else {
            value.str()?.to_str()?.to_string()
        };
        out.insert(key, value);
    }
    Ok(out)
}

fn py_string_list(value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<Vec<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_none() {
        return Ok(None);
    }
    if let Ok(s) = value.extract::<String>() {
        return Ok(Some(split_csv(&s)));
    }
    if let Ok(list) = value.cast::<PyList>() {
        return collect_py_string_iter(list.iter()).map(Some);
    }
    if let Ok(tuple) = value.cast::<PyTuple>() {
        return collect_py_string_iter(tuple.iter()).map(Some);
    }
    Ok(Some(vec![py_value_to_string(value)?]))
}

fn collect_py_string_iter<'py>(
    iter: impl Iterator<Item = Bound<'py, PyAny>>,
) -> PyResult<Vec<String>> {
    let mut out = Vec::new();
    for item in iter {
        let s = py_value_to_string(&item)?;
        if !s.is_empty() {
            out.push(s);
        }
    }
    Ok(out)
}

fn py_value_to_string(value: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(s) = value.extract::<String>() {
        return Ok(s);
    }
    for attr in ["Value", "value", "Ticker", "ticker"] {
        if let Ok(v) = value.getattr(attr) {
            if let Ok(s) = v.extract::<String>() {
                return Ok(s);
            }
        }
    }
    Ok(value.str()?.to_str()?.to_string())
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn custom_query_from_properties(
    properties: &HashMap<String, String>,
) -> lean_data::CustomDataQuery {
    let mut query = lean_data::CustomDataQuery::default();
    if let Some(symbols) = properties.get("symbols") {
        query.symbols = Some(split_csv(symbols));
    }
    if let Some(columns) = properties.get("columns") {
        query.columns = Some(split_csv(columns));
    }
    for (key, value) in properties {
        if let Some(column) = key.strip_prefix("eq_") {
            query
                .string_equals
                .insert(column.to_string(), value.to_string());
        } else if let Some(column) = key.strip_prefix("in_") {
            query.string_in.insert(column.to_string(), split_csv(value));
        } else if let Some(column) = key.strip_prefix("min_") {
            if let Ok(v) = value.parse::<f64>() {
                query.numeric_min.insert(column.to_string(), v);
            }
        } else if let Some(column) = key.strip_prefix("max_") {
            if let Ok(v) = value.parse::<f64>() {
                query.numeric_max.insert(column.to_string(), v);
            }
        }
    }
    query.properties = properties.clone();
    query
}

impl Default for PyQcAlgorithm {
    fn default() -> Self {
        Self::new()
    }
}

#[pymethods]
impl PyQcAlgorithm {
    #[new]
    pub fn new() -> Self {
        PyQcAlgorithm {
            inner: Arc::new(Mutex::new(QcAlgorithm::new(
                "PythonStrategy",
                dec!(100_000),
            ))),
            symbols: HashMap::new(),
            charts: Arc::new(Mutex::new(ChartCollection::new())),
            framework: Arc::new(Mutex::new(FrameworkState::new())),
            indicators: Arc::new(Mutex::new(IndicatorRegistry::new())),
            universe_settings: PyUniverseSettings::new_shared(),
            universes: Arc::new(Mutex::new(Vec::new())),
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

    /// Set the benchmark symbol.  When not called, SPY is used as the default.
    ///
    /// ```python
    /// self.set_benchmark("QQQ")
    /// ```
    fn set_benchmark(&mut self, ticker: &str) {
        self.inner.lock().unwrap().set_benchmark(ticker);
    }

    /// Set the warm-up period.
    ///
    /// If `bars_or_days` > 365 it is treated as a bar count; otherwise as a
    /// number of calendar days (which is consistent with C# LEAN's overloads).
    ///
    /// Examples (Python):
    ///   self.set_warm_up(200)   # 200 bars
    ///   self.set_warm_up(30)    # 30 days
    #[pyo3(signature = (bars_or_days_or_timespan, resolution=None))]
    fn set_warm_up(
        &mut self,
        bars_or_days_or_timespan: &Bound<'_, PyAny>,
        resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        // C# LEAN has two overload families:
        //   SetWarmUp(int barCount[, Resolution resolution])
        //     → count back N trading-day bars from start date
        //   SetWarmUp(TimeSpan timeSpan[, Resolution resolution])
        //     → subtract the span directly (calendar time, not trading days)
        //
        // Python passes either an int (bar count or day count) or a timedelta.
        // When a Resolution is provided the int is always a bar count.
        // When no Resolution is provided and the int is ≤ 365 it is treated as
        // a TimeSpan of that many calendar days (legacy rlean snake_case behaviour).
        use lean_core::TimeSpan;

        // Check for timedelta first.
        if let Ok(td) = bars_or_days_or_timespan.extract::<chrono::Duration>() {
            let nanos = td.num_nanoseconds().unwrap_or(0);
            self.inner
                .lock()
                .unwrap()
                .set_warm_up(TimeSpan::from_nanos(nanos));
            return Ok(());
        }

        let n: i64 = bars_or_days_or_timespan.extract()?;

        // With a resolution argument this is always a bar count (C# overload).
        // Without a resolution, > 365 is a bar count; ≤ 365 is calendar days.
        if resolution.is_some() || n > 365 {
            // Bar count: stored as warmup_bar_count; runner converts to calendar days.
            self.inner.lock().unwrap().set_warm_up_bars(n as usize);
        } else {
            // Calendar days (TimeSpan overload without resolution).
            let nanos = n * 86_400 * 1_000_000_000i64;
            self.inner
                .lock()
                .unwrap()
                .set_warm_up(TimeSpan::from_nanos(nanos));
        }
        Ok(())
    }

    // ─── Universe ─────────────────────────────────────────────────────────────

    fn add_equity(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let sym = self.inner.lock().unwrap().add_equity(ticker, res);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_symbol(PySymbol { inner: sym })
    }

    fn add_forex(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let sym = self.inner.lock().unwrap().add_forex(ticker, res);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_symbol(PySymbol { inner: sym })
    }

    fn add_crypto(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let market = Market::usa(); // default; crypto can override
        let sym = self.inner.lock().unwrap().add_crypto(ticker, &market, res);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_symbol(PySymbol { inner: sym })
    }

    #[getter]
    fn universe_settings(&self) -> PyUniverseSettings {
        self.universe_settings.clone()
    }

    #[getter]
    fn date_rules(&self) -> PyDateRules {
        PyDateRules::default()
    }

    #[getter]
    fn time_rules(&self) -> PyTimeRules {
        PyTimeRules::default()
    }

    #[pyo3(signature = (*args))]
    fn add_universe(&mut self, py: Python<'_>, args: &Bound<'_, PyTuple>) -> PyResult<()> {
        if args.len() == 1 {
            let universe = args.get_item(0)?.extract::<Py<PyScheduledUniverse>>()?;
            self.universes.lock().unwrap().push(universe);
            return Ok(());
        }

        if args.len() >= 4 {
            let ticker = args.get_item(1)?.extract::<String>()?;
            let resolution = args.get_item(2)?.extract::<PyResolution>()?;
            let selector = args.get_item(3)?.unbind();
            let universe = PyScheduledUniverse::custom_data(
                ticker,
                selector,
                resolution.into(),
                self.universe_settings.snapshot(),
            );
            self.universes.lock().unwrap().push(Py::new(py, universe)?);
            return Ok(());
        }

        if args.len() >= 3 {
            let resolution = args.get_item(1)?.extract::<PyResolution>()?;
            let selector = args.get_item(2)?.unbind();
            let universe = PyScheduledUniverse::user_defined(
                selector,
                resolution.into(),
                self.universe_settings.snapshot(),
            );
            self.universes.lock().unwrap().push(Py::new(py, universe)?);
            return Ok(());
        }

        Err(pyo3::exceptions::PyTypeError::new_err(
            "add_universe expects ScheduledUniverse, (name, resolution, selector), or (source, name, resolution, selector)",
        ))
    }

    // ─── Ordering ─────────────────────────────────────────────────────────────

    /// LEAN API: place a market order. Routes option symbols through the option
    /// position manager using the current chain mid price.
    fn market_order(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        if sym.option_symbol_id().is_some() {
            self.option_market_order(sym, f2d(quantity))
        } else {
            self.inner.lock().unwrap().market_order(&sym, f2d(quantity));
            Ok(())
        }
    }

    /// LEAN API: `self.buy(symbol, quantity)` — market buy.
    fn buy(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<()> {
        self.market_order(symbol, quantity.abs())
    }

    /// LEAN API: `self.sell(symbol, quantity)` — market sell.
    fn sell(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<()> {
        self.market_order(symbol, -quantity.abs())
    }

    /// LEAN API: `self.order(symbol, quantity)` — alias for market_order.
    fn order(&mut self, symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<()> {
        self.market_order(symbol, quantity)
    }

    /// Place a limit order.
    fn limit_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        limit_price: f64,
    ) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        self.inner
            .lock()
            .unwrap()
            .limit_order(&sym, f2d(quantity), f2d(limit_price));
        Ok(())
    }

    /// Place a stop-market order.
    fn stop_market_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        stop_price: f64,
    ) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        self.inner
            .lock()
            .unwrap()
            .stop_market_order(&sym, f2d(quantity), f2d(stop_price));
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

    /// LEAN API: exercise a long option position.
    fn exercise_option(&mut self, symbol: &Bound<'_, PyAny>) -> PyResult<()> {
        let sym = self.resolve_symbol(symbol)?;
        tracing::info!("Exercise option: {}", sym.value);
        // Actual exercise is handled by the runner at expiry; this is a no-op
        // for strategies that call it before expiry (LEAN ignores early exercise for Americans in backtests).
        Ok(())
    }

    // ─── Custom Data ──────────────────────────────────────────────────────────

    /// LEAN API: `self.add_data(source_type, ticker, resolution=Resolution.DAILY, properties={...})`.
    ///
    /// Registers a custom data subscription so the runner fetches and delivers
    /// data points to `on_data` via `data.custom[ticker]`.
    ///
    /// ```python
    /// self.unrate = self.add_data("fred", "UNRATE").symbol
    /// self.vix    = self.add_data("cboe_vix", "VIX", Resolution.DAILY)
    /// ```
    #[pyo3(signature = (source_type, ticker, resolution=None, properties=None))]
    fn add_data(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Option<&Bound<'_, PyAny>>,
        properties: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<PySecurity> {
        use lean_core::Resolution;
        use lean_data::{CustomDataConfig, CustomDataQuery, CustomDataSubscription};

        let res = match resolution {
            Some(r) => {
                if let Ok(py_res) = r.extract::<PyResolution>() {
                    Resolution::from(py_res)
                } else if let Ok(s) = r.extract::<String>() {
                    match s.to_lowercase().as_str() {
                        "tick" => Resolution::Tick,
                        "second" => Resolution::Second,
                        "daily" => Resolution::Daily,
                        "hour" => Resolution::Hour,
                        "minute" => Resolution::Minute,
                        _ => Resolution::Daily,
                    }
                } else {
                    Resolution::Daily
                }
            }
            None => Resolution::Daily,
        };

        let properties = py_properties_to_map(properties)?;
        let query = custom_query_from_properties(&properties);

        let config = CustomDataConfig {
            ticker: ticker.to_string(),
            source_type: source_type.to_string(),
            resolution: res,
            properties,
            query,
        };
        let sub = CustomDataSubscription {
            source_type: source_type.to_string(),
            ticker: ticker.to_string(),
            config,
            dynamic_query: CustomDataQuery::default(),
        };

        self.inner
            .lock()
            .unwrap()
            .custom_data_subscriptions
            .push(sub);

        // Return a synthetic security object so callers can do:
        //   self.unrate = self.add_data("fred", "UNRATE").symbol
        let market = lean_core::Market::usa();
        let sym = lean_core::Symbol::create_equity(ticker, &market);
        self.symbols.insert(ticker.to_uppercase(), sym.clone());
        Ok(PySecurity::from_symbol(PySymbol { inner: sym }))
    }

    /// Update dynamic custom-data query hints for an existing subscription.
    ///
    /// This is intended for evolving universes: broad custom data can be used
    /// to select a universe, then downstream custom subscriptions can be
    /// narrowed to the current active symbols.
    #[pyo3(signature = (source_type, ticker, symbols=None, columns=None, properties=None))]
    fn set_custom_data_query(
        &mut self,
        source_type: &str,
        ticker: &str,
        symbols: Option<&Bound<'_, PyAny>>,
        columns: Option<&Bound<'_, PyAny>>,
        properties: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<()> {
        let mut query = lean_data::CustomDataQuery {
            symbols: py_string_list(symbols)?,
            columns: py_string_list(columns)?,
            ..Default::default()
        };
        let properties = py_properties_to_map(properties)?;
        query = query.merge(&custom_query_from_properties(&properties));
        query.properties.extend(properties);
        let mut inner = self.inner.lock().unwrap();
        for sub in &mut inner.custom_data_subscriptions {
            if sub.source_type.eq_ignore_ascii_case(source_type)
                && sub.ticker.eq_ignore_ascii_case(ticker)
            {
                sub.dynamic_query = query;
                return Ok(());
            }
        }
        Err(pyo3::exceptions::PyValueError::new_err(format!(
            "custom data subscription not found: {source_type}/{ticker}"
        )))
    }

    #[pyo3(signature = (source_type, ticker, symbols))]
    fn set_custom_data_symbols(
        &mut self,
        source_type: &str,
        ticker: &str,
        symbols: &Bound<'_, PyAny>,
    ) -> PyResult<()> {
        self.set_custom_data_query(source_type, ticker, Some(symbols), None, None)
    }

    // ─── Options ──────────────────────────────────────────────────────────────

    /// Subscribe to an option chain for an underlying equity.
    /// Returns a LEAN-compatible `Option` security object with `.symbol` and `.set_filter()`.
    /// Accepts `Resolution.DAILY`, `Resolution.Daily`, etc. or a string, defaulting to Daily.
    #[pyo3(signature = (ticker, resolution=None))]
    fn add_option(
        &mut self,
        ticker: &str,
        resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<crate::py_types::PyOptionSecurity> {
        use lean_core::Resolution;
        let res = match resolution {
            Some(r) => {
                if let Ok(py_res) = r.extract::<PyResolution>() {
                    Resolution::from(py_res)
                } else if let Ok(s) = r.extract::<String>() {
                    match s.to_lowercase().as_str() {
                        "tick" => Resolution::Tick,
                        "second" => Resolution::Second,
                        "daily" => Resolution::Daily,
                        "hour" => Resolution::Hour,
                        "minute" => Resolution::Minute,
                        _ => Resolution::Daily,
                    }
                } else {
                    Resolution::Daily
                }
            }
            None => Resolution::Daily,
        };
        let canonical = self.inner.lock().unwrap().add_option(ticker, res);
        Ok(crate::py_types::PyOptionSecurity {
            canonical: crate::py_types::PySymbol { inner: canonical },
        })
    }

    // ─── Securities ───────────────────────────────────────────────────────────

    /// LEAN API: `self.securities[symbol]` — returns the Security for a symbol.
    #[getter]
    fn securities(&self) -> PySecurityManager {
        let alg = self.inner.lock().unwrap();
        let mut entries = HashMap::new();
        for sec in alg.securities.all() {
            let sid = sec.symbol.id.sid;
            entries.insert(
                sid,
                PySecurityManager::build_entry(
                    sec.symbol.clone(),
                    sec.current_price().to_f64().unwrap_or(0.0),
                ),
            );
        }
        PySecurityManager::from_entries(entries)
    }

    // ─── Portfolio ────────────────────────────────────────────────────────────

    #[getter]
    fn portfolio(&self) -> PyPortfolio {
        let inner = self.inner.lock().unwrap();
        PyPortfolio {
            inner: inner.portfolio.clone(),
        }
    }

    #[getter]
    fn cash(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.inner.lock().unwrap().cash().to_f64().unwrap_or(0.0)
    }

    #[getter]
    fn portfolio_value(&self) -> f64 {
        use rust_decimal::prelude::ToPrimitive;
        self.inner
            .lock()
            .unwrap()
            .portfolio_value()
            .to_f64()
            .unwrap_or(0.0)
    }

    fn is_invested(&self, symbol: &Bound<'_, PyAny>) -> PyResult<bool> {
        let sym = self.resolve_symbol(symbol)?;
        Ok(self.inner.lock().unwrap().is_invested(&sym))
    }

    // ─── Time ────────────────────────────────────────────────────────────────

    /// Current algorithm time as a Python datetime object (matches LEAN's `self.time`).
    #[getter]
    fn time(&self) -> PyResult<Py<PyAny>> {
        let ns = self.inner.lock().unwrap().time.0;
        ns_to_py_datetime_in_tz(ns, chrono_tz::America::New_York)
    }

    /// Current UTC time as a Python datetime object.
    #[getter]
    fn utc_time(&self) -> PyResult<Py<PyAny>> {
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

    /// LEAN API: `self.error(message)` — log an error-level message.
    fn error(&self, message: &str) {
        tracing::error!("Algorithm: {message}");
        self.inner
            .lock()
            .unwrap()
            .log_message(format!("ERROR: {message}"));
    }

    // ─── Market Hours ─────────────────────────────────────────────────────────

    /// LEAN API: `self.is_market_open(symbol)` — always True in daily-resolution backtests.
    #[pyo3(signature = (symbol=None))]
    fn is_market_open(&self, symbol: Option<&Bound<'_, PyAny>>) -> bool {
        let _ = symbol;
        true
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

    // ─── Algorithm Framework ─────────────────────────────────────────────────

    /// Register an alpha model. Multiple calls add models to a composite.
    /// ```python
    /// self.add_alpha(EmaCrossAlphaModel(50, 200))
    /// self.add_alpha(RsiAlphaModel(14))
    /// ```
    fn add_alpha(slf: Bound<'_, Self>, model: &Bound<'_, PyAny>) {
        let alg_py: Py<PyAny> = slf.clone().into_any().unbind();
        let fw = slf.borrow().framework.clone();
        if let Some(m) = try_take_alpha(model, alg_py) {
            fw.lock().unwrap().alpha_models.push(m);
        }
    }

    /// Set the portfolio construction model.
    /// ```python
    /// self.set_portfolio_construction(EqualWeightingPortfolioConstructionModel())
    /// ```
    fn set_portfolio_construction(slf: Bound<'_, Self>, model: &Bound<'_, PyAny>) {
        let alg_py: Py<PyAny> = slf.clone().into_any().unbind();
        let fw = slf.borrow().framework.clone();
        if let Some(m) = try_take_pcm(model, alg_py) {
            fw.lock().unwrap().pcm = m;
        }
    }

    /// Set the execution model.
    /// ```python
    /// self.set_execution(ImmediateExecutionModel())
    /// ```
    fn set_execution(&mut self, model: &Bound<'_, PyAny>) {
        if let Some(m) = try_take_exec(model) {
            self.framework.lock().unwrap().exec_model = m;
        }
    }

    /// Set the risk management model.
    /// ```python
    /// self.set_risk_management(MaximumDrawdownPercentPerSecurity(0.05))
    /// ```
    fn set_risk_management(&mut self, model: &Bound<'_, PyAny>) {
        if let Some(m) = try_take_risk(model) {
            self.framework.lock().unwrap().risk_model = m;
        }
    }

    // ─── Algorithm settings ───────────────────────────────────────────────────

    /// LEAN API: `self.Settings` — returns a settings bag (no-op in rlean).
    #[getter]
    fn settings(&self) -> PyAlgorithmSettings {
        PyAlgorithmSettings::new()
    }

    // ─── Indicator factory methods ────────────────────────────────────────────
    // LEAN API: self.SMA(symbol, period, resolution) etc.
    // Creates the indicator, registers it for auto-update each bar, returns it.

    /// `self.SMA(symbol, period[, resolution])` — Simple Moving Average.
    #[pyo3(signature = (symbol, period, _resolution=None))]
    fn sma(
        slf: Bound<'_, Self>,
        symbol: &Bound<'_, PyAny>,
        period: usize,
        _resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PySma>> {
        let sid = resolve_symbol_sid(symbol)?;
        let indicator = Py::new(slf.py(), PySma::create(period))?;
        slf.borrow()
            .indicators
            .lock()
            .unwrap()
            .entries
            .push((sid, indicator.clone_ref(slf.py()).into_any()));
        Ok(indicator)
    }

    /// `self.EMA(symbol, period[, resolution])` — Exponential Moving Average.
    #[pyo3(signature = (symbol, period, _resolution=None))]
    fn ema(
        slf: Bound<'_, Self>,
        symbol: &Bound<'_, PyAny>,
        period: usize,
        _resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyEma>> {
        let sid = resolve_symbol_sid(symbol)?;
        let indicator = Py::new(slf.py(), PyEma::create(period))?;
        slf.borrow()
            .indicators
            .lock()
            .unwrap()
            .entries
            .push((sid, indicator.clone_ref(slf.py()).into_any()));
        Ok(indicator)
    }

    /// `self.RSI(symbol, period[, moving_average_type, resolution])` — RSI.
    #[pyo3(signature = (symbol, period, _moving_average_type=None, _resolution=None))]
    fn rsi(
        slf: Bound<'_, Self>,
        symbol: &Bound<'_, PyAny>,
        period: usize,
        _moving_average_type: Option<&Bound<'_, PyAny>>,
        _resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyRsi>> {
        let sid = resolve_symbol_sid(symbol)?;
        let indicator = Py::new(slf.py(), PyRsi::create(period))?;
        slf.borrow()
            .indicators
            .lock()
            .unwrap()
            .entries
            .push((sid, indicator.clone_ref(slf.py()).into_any()));
        Ok(indicator)
    }

    /// `self.MOMP(symbol, period[, resolution])` — Momentum Percent.
    #[pyo3(signature = (symbol, period, _resolution=None))]
    fn momp(
        slf: Bound<'_, Self>,
        symbol: &Bound<'_, PyAny>,
        period: usize,
        _resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyMomp>> {
        let sid = resolve_symbol_sid(symbol)?;
        let indicator = Py::new(slf.py(), PyMomp::create(period))?;
        slf.borrow()
            .indicators
            .lock()
            .unwrap()
            .entries
            .push((sid, indicator.clone_ref(slf.py()).into_any()));
        Ok(indicator)
    }

    /// `self.STD(symbol, period[, resolution])` — Standard Deviation.
    #[pyo3(signature = (symbol, period, _resolution=None))]
    fn std(
        slf: Bound<'_, Self>,
        symbol: &Bound<'_, PyAny>,
        period: usize,
        _resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Py<PyStd>> {
        let sid = resolve_symbol_sid(symbol)?;
        let indicator = Py::new(slf.py(), PyStd::create(period))?;
        slf.borrow()
            .indicators
            .lock()
            .unwrap()
            .entries
            .push((sid, indicator.clone_ref(slf.py()).into_any()));
        Ok(indicator)
    }

    /// PascalCase → snake_case attribute forwarding so LEAN strategies can call
    /// QCAlgorithm methods by their LEAN names (e.g. `self.SetStartDate(...)`).
    /// Called only when normal attribute lookup fails, so snake_case always wins
    /// for directly defined methods/properties.
    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'QCAlgorithm' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        let inner = self.inner.lock().unwrap();
        format!(
            "QCAlgorithm(name='{}', value={:.2})",
            inner.name,
            inner.portfolio_value()
        )
    }
}

/// Resolve a symbol/security/string argument to its SID (for indicator registry).
fn resolve_symbol_sid(sym: &Bound<'_, PyAny>) -> PyResult<u64> {
    use crate::py_types::{PySecurity, PySymbol};
    if let Ok(s) = sym.cast::<PySymbol>() {
        return Ok(s.get().inner.id.sid);
    }
    if let Ok(s) = sym.cast::<PySecurity>() {
        return Ok(s.get().inner.inner.id.sid);
    }
    if let Ok(ticker) = sym.extract::<String>() {
        let s = lean_core::Symbol::create_equity(&ticker, &Market::usa());
        return Ok(s.id.sid);
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "Expected Symbol, Security, or str",
    ))
}

/// Convert PascalCase / CamelCase to snake_case.
/// e.g. "SetStartDate" → "set_start_date", "TotalPortfolioValue" → "total_portfolio_value"
pub(crate) fn pascal_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let chars: Vec<char> = name.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                // Insert _ unless the previous char was already _ or uppercase
                // (handles acronyms like "IV" → "iv" not "i_v")
                let prev = chars[i - 1];
                let next_is_lower = chars.get(i + 1).map(|c| c.is_lowercase()).unwrap_or(false);
                if prev != '_' && (prev.is_lowercase() || next_is_lower) {
                    out.push('_');
                }
            }
            out.push(c.to_lowercase().next().unwrap());
        } else {
            out.push(c);
        }
    }
    out
}

fn ns_to_py_datetime(ns: i64) -> PyResult<Py<PyAny>> {
    Python::attach(|py| {
        let secs = ns / 1_000_000_000;
        let micros = (ns % 1_000_000_000) / 1_000;
        let timestamp = secs as f64 + micros as f64 / 1_000_000.0;
        let datetime = py
            .import("datetime")?
            .getattr("datetime")?
            .call_method1("utcfromtimestamp", (timestamp,))?;
        Ok(datetime.into())
    })
}

fn ns_to_py_datetime_in_tz(ns: i64, tz: chrono_tz::Tz) -> PyResult<Py<PyAny>> {
    Python::attach(|py| {
        use chrono::{DateTime as ChronoDateTime, Utc};
        let secs = ns / 1_000_000_000;
        let nsub = (ns % 1_000_000_000) as u32;
        let dt: ChronoDateTime<Utc> =
            chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
        let local = dt.with_timezone(&tz).naive_local();
        let datetime = py.import("datetime")?.getattr("datetime")?.call1((
            local.year(),
            local.month(),
            local.day(),
            local.hour(),
            local.minute(),
            local.second(),
            local.and_utc().timestamp_subsec_micros(),
        ))?;
        Ok(datetime.into())
    })
}

fn lean_datetime_to_iso(ns: i64) -> String {
    use chrono::{DateTime as ChronoDateTime, Utc};
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Format a nanosecond timestamp as "YYYY-MM-DD" for chart points.
fn lean_datetime_to_date(ns: i64) -> String {
    use chrono::{DateTime as ChronoDateTime, Utc};
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    dt.format("%Y-%m-%d").to_string()
}

impl PyQcAlgorithm {
    fn resolve_symbol(&self, arg: &Bound<'_, PyAny>) -> PyResult<lean_core::Symbol> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(sym.get().inner.clone());
        }
        // Accept Security objects directly (mirrors LEAN's set_holdings(security, ...) API)
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(sec.get().inner.inner.clone());
        }
        // Accept OptionContract objects — uses contract.symbol
        if let Ok(contract) = arg.cast::<crate::py_options::PyOptionContract>() {
            return Ok(contract.borrow().inner.symbol.clone());
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
            "Expected Security, Symbol, OptionContract, or ticker string",
        ))
    }

    /// Route a market order for an option symbol through the option position manager.
    /// Looks up the mid price from the current option chain.
    fn option_market_order(&mut self, sym: lean_core::Symbol, quantity: Decimal) -> PyResult<()> {
        // Determine canonical permtick: ?UNDERLYING
        let canonical = sym
            .underlying
            .as_ref()
            .map(|u| format!("?{}", u.permtick))
            .unwrap_or_default();

        // Look up mid price from option chains (keyed by Symbol, match by SID)
        let sid = sym.id.sid;
        let premium = {
            let alg = self.inner.lock().unwrap();
            alg.option_chains
                .get(&canonical)
                .and_then(|chain| chain.contracts.iter().find(|(s, _)| s.id.sid == sid))
                .map(|(_, c)| c.mid_price())
                .unwrap_or(Decimal::ZERO)
        };

        // Determine if opening or closing from the portfolio holding, mirroring LEAN.
        let existing_qty = {
            self.inner
                .lock()
                .unwrap()
                .portfolio
                .get_holding(&sym)
                .quantity
        };

        let abs_qty = quantity.abs();
        if quantity < Decimal::ZERO {
            if existing_qty > Decimal::ZERO {
                self.inner
                    .lock()
                    .unwrap()
                    .sell_to_close(sym, abs_qty, premium);
            } else {
                self.inner
                    .lock()
                    .unwrap()
                    .sell_to_open(sym, abs_qty, premium);
            }
        } else if quantity > Decimal::ZERO {
            if existing_qty < Decimal::ZERO {
                self.inner
                    .lock()
                    .unwrap()
                    .buy_to_close(sym, abs_qty, premium);
            } else {
                self.inner
                    .lock()
                    .unwrap()
                    .buy_to_open(sym, abs_qty, premium);
            }
        }
        Ok(())
    }
}
