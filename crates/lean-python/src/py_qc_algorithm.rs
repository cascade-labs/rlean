use crate::charting::ChartCollection;
use crate::py_data::PyTradeBar;
use crate::py_framework::{
    try_take_alpha, try_take_exec, try_take_pcm, try_take_risk, FrameworkState,
};
use crate::py_indicators::{PyEma, PyMomp, PyRsi, PySma, PyStd};
use crate::py_orders::PyOrderTicket;
use crate::py_portfolio::PyPortfolio;
use crate::py_types::{
    PyAlgorithmSettings, PyDataNormalizationMode, PyOptionSecurity, PyResolution, PySecurity,
    PySecurityManager, PySymbol,
};
use crate::py_universe::{PyDateRules, PyScheduledUniverse, PyTimeRules, PyUniverseSettings};
use crate::{PyAccountType, PyBrokerageName, PySecurityType, PyTimeInForce};
use chrono::{Datelike, NaiveDate, TimeZone, Timelike};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{
    DataNormalizationMode, DateTime, Market, Resolution, SecurityType, Symbol, TickType,
};
use lean_data::{CustomDataPoint, CustomDataSubscription, TradeBar};
use lean_engine::HistoryService;
use lean_options::{implied_volatility, time_to_expiry_years};
use lean_orders::order::TimeInForce;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

pub use lean_engine::AlgorithmHistoryContext;

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

/// LEAN API: seed a newly-added security from a Python history function.
#[pyclass(name = "FuncSecuritySeeder")]
pub struct PyFuncSecuritySeeder {
    seed_function: Py<PyAny>,
}

#[pymethods]
impl PyFuncSecuritySeeder {
    #[new]
    fn new(seed_function: Py<PyAny>) -> Self {
        Self { seed_function }
    }

    fn seed_security(&self, py: Python<'_>, security: &Bound<'_, PyAny>) -> PyResult<bool> {
        let result = self.seed_function.call1(py, (security,))?;
        seed_security_from_result(py, security, result.bind(py))
    }

    #[pyo3(name = "SeedSecurity")]
    fn seed_security_pascal(&self, py: Python<'_>, security: &Bound<'_, PyAny>) -> PyResult<bool> {
        self.seed_security(py, security)
    }
}

/// LEAN API: security initializer that delegates price seeding to an ISecuritySeeder.
#[pyclass(name = "BrokerageModelSecurityInitializer")]
pub struct PyBrokerageModelSecurityInitializer {
    security_seeder: Option<Py<PyAny>>,
}

#[pymethods]
impl PyBrokerageModelSecurityInitializer {
    #[new]
    #[pyo3(signature = (brokerage_model=None, security_seeder=None))]
    fn new(
        _py: Python<'_>,
        brokerage_model: Option<Py<PyAny>>,
        security_seeder: Option<Py<PyAny>>,
    ) -> Self {
        let _ = brokerage_model;
        Self { security_seeder }
    }

    fn initialize(&self, py: Python<'_>, security: &Bound<'_, PyAny>) -> PyResult<()> {
        if let Some(seeder) = &self.security_seeder {
            seeder.call_method1(py, "seed_security", (security,))?;
        }
        Ok(())
    }

    #[pyo3(name = "Initialize")]
    fn initialize_pascal(&self, py: Python<'_>, security: &Bound<'_, PyAny>) -> PyResult<()> {
        self.initialize(py, security)
    }
}

fn seed_security_from_result(
    _py: Python<'_>,
    security: &Bound<'_, PyAny>,
    result: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    if result.is_none() {
        return Ok(false);
    }

    let iter = result.try_iter().map_err(|_| {
        pyo3::exceptions::PyTypeError::new_err(
            "security seeder must return None or an iterable of TradeBar values",
        )
    })?;

    let mut got_data = false;
    for item in iter {
        let item = item?;
        let seeded = security
            .call_method1("set_market_price", (&item,))?
            .extract::<bool>()?;
        got_data |= seeded;
    }
    Ok(got_data)
}

impl Default for IndicatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn f2d(f: f64) -> Decimal {
    Decimal::from_f64(f).unwrap_or_default()
}

fn py_time_in_force(value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<TimeInForce>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_none() {
        return Ok(None);
    }
    if let Ok(time_in_force) = value.extract::<PyTimeInForce>() {
        return Ok(Some(time_in_force.into()));
    }
    if let Ok(text) = value.extract::<String>() {
        let normalized = text
            .trim()
            .replace([' ', '-', '_'], "")
            .to_ascii_lowercase();
        return match normalized.as_str() {
            "day" => Ok(Some(TimeInForce::Day)),
            "gtc" | "goodtilcanceled" | "goodtilcancelled" => {
                Ok(Some(TimeInForce::GoodTilCanceled))
            }
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unsupported time_in_force: {text}"
            ))),
        };
    }
    Err(pyo3::exceptions::PyValueError::new_err(
        "time_in_force must be TimeInForce.DAY, TimeInForce.GTC, 'day', or 'gtc'",
    ))
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
    pub symbols: Arc<Mutex<HashMap<String, lean_core::Symbol>>>,
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
    /// Runtime data context installed by the runner before Initialize().
    pub history_context: Arc<Mutex<Option<AlgorithmHistoryContext>>>,
    /// Runtime algorithm parameters supplied by the CLI/config before Initialize().
    pub parameters: Arc<Mutex<HashMap<String, String>>>,
    /// Optional LEAN-style initializer applied to newly-created securities.
    pub security_initializer: Arc<Mutex<Option<Py<PyAny>>>>,
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
    pub fn set_parameters(&self, parameters: HashMap<String, String>) {
        *self.parameters.lock().unwrap() = parameters;
    }
    pub fn set_history_context(&self, context: AlgorithmHistoryContext) {
        *self.history_context.lock().unwrap() = Some(context);
    }

    fn initialize_security_from_python(
        &self,
        py: Python<'_>,
        security: &PySecurity,
    ) -> PyResult<()> {
        let initializer = self
            .security_initializer
            .lock()
            .unwrap()
            .as_ref()
            .map(|initializer| initializer.clone_ref(py));
        if let Some(initializer) = initializer {
            initializer.call_method1(py, "initialize", (security.clone(),))?;
        }
        Ok(())
    }

    fn register_custom_data_subscription(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
        role: lean_data::CustomDataSubscriptionRole,
    ) {
        let query = custom_query_from_properties(&properties);
        let config = lean_data::CustomDataConfig {
            ticker: ticker.to_string(),
            source_type: source_type.to_string(),
            resolution,
            properties,
            query,
        };
        let sub = lean_data::CustomDataSubscription {
            source_type: source_type.to_string(),
            ticker: ticker.to_string(),
            config,
            dynamic_query: lean_data::CustomDataQuery::default(),
            role,
        };

        let mut inner = self.inner.lock().unwrap();
        if let Some(existing) = inner.custom_data_subscriptions.iter_mut().find(|existing| {
            existing.source_type.eq_ignore_ascii_case(source_type)
                && existing.ticker.eq_ignore_ascii_case(ticker)
        }) {
            *existing = sub;
        } else {
            inner.custom_data_subscriptions.push(sub);
        }
    }

    fn register_custom_universe_subscription(
        &mut self,
        source_type: &str,
        ticker: &str,
        resolution: Resolution,
        properties: HashMap<String, String>,
    ) {
        self.register_custom_data_subscription(
            source_type,
            ticker,
            resolution,
            properties,
            lean_data::CustomDataSubscriptionRole::Universe,
        );
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

fn security_type_from_py(value: &Bound<'_, PyAny>) -> PyResult<SecurityType> {
    if let Ok(py_type) = value.extract::<PySecurityType>() {
        return Ok(match py_type {
            PySecurityType::Base => SecurityType::Base,
            PySecurityType::Equity => SecurityType::Equity,
            PySecurityType::Option => SecurityType::Option,
            PySecurityType::Forex => SecurityType::Forex,
            PySecurityType::Future => SecurityType::Future,
            PySecurityType::Cfd => SecurityType::Cfd,
            PySecurityType::Crypto => SecurityType::Crypto,
            PySecurityType::Index => SecurityType::Index,
            PySecurityType::IndexOption => SecurityType::IndexOption,
            PySecurityType::CryptoFuture => SecurityType::CryptoFuture,
        });
    }
    if let Ok(raw) = value.extract::<String>() {
        return match raw.trim().to_ascii_lowercase().as_str() {
            "base" => Ok(SecurityType::Base),
            "equity" => Ok(SecurityType::Equity),
            "option" => Ok(SecurityType::Option),
            "forex" => Ok(SecurityType::Forex),
            "future" => Ok(SecurityType::Future),
            "cfd" => Ok(SecurityType::Cfd),
            "crypto" => Ok(SecurityType::Crypto),
            "index" => Ok(SecurityType::Index),
            "indexoption" | "index_option" | "index-option" => Ok(SecurityType::IndexOption),
            "cryptofuture" | "crypto_future" | "crypto-future" => Ok(SecurityType::CryptoFuture),
            _ => Err(pyo3::exceptions::PyValueError::new_err(format!(
                "unsupported SecurityType '{raw}'"
            ))),
        };
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "expected SecurityType or security type name",
    ))
}

fn normalize_hyperliquid_universe(value: &str) -> String {
    let cleaned = value
        .trim()
        .replace(['-', '.', ':', ' '], "_")
        .to_ascii_uppercase();
    match cleaned.as_str() {
        "PERP" | "PERPS" | "CRYPTOFUTURE" | "CRYPTO_FUTURE" | "CRYPTO_PERPS" => {
            "CRYPTO_PERP".to_string()
        }
        "SPOT" | "CRYPTO" => "CRYPTO_SPOT".to_string(),
        "HIP3_TRADING_XYZ" => "HIP3_XYZ".to_string(),
        other => other.to_string(),
    }
}

fn hyperliquid_universe_security_type(universe: &str) -> SecurityType {
    if universe == "CRYPTO_SPOT" {
        SecurityType::Crypto
    } else {
        SecurityType::CryptoFuture
    }
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
            symbols: Arc::new(Mutex::new(HashMap::new())),
            charts: Arc::new(Mutex::new(ChartCollection::new())),
            framework: Arc::new(Mutex::new(FrameworkState::new())),
            indicators: Arc::new(Mutex::new(IndicatorRegistry::new())),
            universe_settings: PyUniverseSettings::new_shared(),
            universes: Arc::new(Mutex::new(Vec::new())),
            history_context: Arc::new(Mutex::new(None)),
            parameters: Arc::new(Mutex::new(HashMap::new())),
            security_initializer: Arc::new(Mutex::new(None)),
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

    #[pyo3(signature = (brokerage, account_type=PyAccountType::Margin))]
    fn set_brokerage_model(&mut self, brokerage: PyBrokerageName, account_type: PyAccountType) {
        self.inner
            .lock()
            .unwrap()
            .set_brokerage_model(brokerage.into(), account_type.into());
    }

    #[getter]
    fn brokerage_model(&self, py: Python<'_>) -> Py<PyAny> {
        py.None()
    }

    #[getter(BrokerageModel)]
    fn brokerage_model_pascal(&self, py: Python<'_>) -> Py<PyAny> {
        self.brokerage_model(py)
    }

    fn set_security_initializer(&mut self, initializer: Py<PyAny>) {
        *self.security_initializer.lock().unwrap() = Some(initializer);
    }

    #[pyo3(name = "SetSecurityInitializer")]
    fn set_security_initializer_pascal(&mut self, initializer: Py<PyAny>) {
        self.set_security_initializer(initializer);
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

    #[pyo3(signature = (name, default=None))]
    fn get_parameter(&self, name: &str, default: Option<&Bound<'_, PyAny>>) -> Option<String> {
        self.parameters
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .or_else(|| {
                default.and_then(|value| value.str().ok().map(|s| s.to_string_lossy().into_owned()))
            })
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

        let warmup_resolution = resolution
            .map(|value| value.extract::<PyResolution>().map(Resolution::from))
            .transpose()?;

        // Check for timedelta first.
        if let Ok(td) = bars_or_days_or_timespan.extract::<chrono::Duration>() {
            let nanos = td.num_nanoseconds().unwrap_or(0);
            self.inner
                .lock()
                .unwrap()
                .set_warm_up_with_resolution(TimeSpan::from_nanos(nanos), warmup_resolution);
            return Ok(());
        }

        let n: i64 = bars_or_days_or_timespan.extract()?;

        // With a resolution argument this is always a bar count (C# overload).
        // Without a resolution, > 365 is a bar count; ≤ 365 is calendar days.
        if warmup_resolution.is_some() || n > 365 {
            // Bar count: stored as warmup_bar_count; runner converts to calendar days.
            self.inner
                .lock()
                .unwrap()
                .set_warm_up_bars_with_resolution(n as usize, warmup_resolution);
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

    #[pyo3(signature = (
        ticker,
        resolution,
        market=None,
        fill_forward=true,
        leverage=None,
        extended_market_hours=false,
        data_normalization_mode=None,
        dataNormalizationMode=None
    ))]
    #[allow(clippy::too_many_arguments)]
    #[allow(non_snake_case)]
    fn add_equity(
        &self,
        py: Python<'_>,
        ticker: &str,
        resolution: PyResolution,
        market: Option<&Bound<'_, PyAny>>,
        fill_forward: bool,
        leverage: Option<f64>,
        extended_market_hours: bool,
        data_normalization_mode: Option<PyDataNormalizationMode>,
        dataNormalizationMode: Option<PyDataNormalizationMode>,
    ) -> PyResult<PySecurity> {
        let _ = (market, fill_forward, leverage, extended_market_hours);
        let res: Resolution = resolution.into();
        let pre_symbol = Symbol::create_equity(ticker, &Market::usa());
        let existed = self.inner.lock().unwrap().securities.contains(&pre_symbol);
        // LEAN parity: `normalizationMode ?? UniverseSettings.DataNormalizationMode`.
        let mode = data_normalization_mode
            .or(dataNormalizationMode)
            .map(Into::into)
            .unwrap_or_else(|| self.universe_settings.snapshot().data_normalization_mode);
        let sym = self
            .inner
            .lock()
            .unwrap()
            .add_equity_with_normalization(ticker, res, Some(mode));
        self.symbols
            .lock()
            .unwrap()
            .insert(ticker.to_uppercase(), sym.clone());
        let security =
            PySecurity::from_algorithm_symbol(PySymbol { inner: sym }, self.inner.clone());
        if !existed {
            self.initialize_security_from_python(py, &security)?;
        }
        Ok(security)
    }

    fn add_forex(&mut self, ticker: &str, resolution: PyResolution) -> PySecurity {
        let res: Resolution = resolution.into();
        let sym = self.inner.lock().unwrap().add_forex(ticker, res);
        self.symbols
            .lock()
            .unwrap()
            .insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_algorithm_symbol(PySymbol { inner: sym }, self.inner.clone())
    }

    #[pyo3(signature = (ticker, resolution, market=None))]
    fn add_crypto(
        &mut self,
        ticker: &str,
        resolution: PyResolution,
        market: Option<&str>,
    ) -> PySecurity {
        let res: Resolution = resolution.into();
        let market = market.map(Market::new).unwrap_or_else(|| {
            self.inner
                .lock()
                .unwrap()
                .default_market_for_security(SecurityType::Crypto)
        });
        let sym = self.inner.lock().unwrap().add_crypto(ticker, &market, res);
        self.symbols
            .lock()
            .unwrap()
            .insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_algorithm_symbol(PySymbol { inner: sym }, self.inner.clone())
    }

    #[pyo3(signature = (ticker, resolution, market=None, leverage=None))]
    fn add_crypto_future(
        &mut self,
        ticker: &str,
        resolution: PyResolution,
        market: Option<&str>,
        leverage: Option<f64>,
    ) -> PySecurity {
        let res: Resolution = resolution.into();
        let market = market.map(Market::new).unwrap_or_else(|| {
            self.inner
                .lock()
                .unwrap()
                .default_market_for_security(SecurityType::CryptoFuture)
        });
        if let Some(leverage) = leverage {
            let symbol = Symbol::create_crypto_future(ticker, &market);
            self.inner
                .lock()
                .unwrap()
                .register_security_leverage(&symbol, leverage);
        }
        let sym = self
            .inner
            .lock()
            .unwrap()
            .add_crypto_future(ticker, &market, res);
        self.symbols
            .lock()
            .unwrap()
            .insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_algorithm_symbol(PySymbol { inner: sym }, self.inner.clone())
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

        if args.len() >= 5 {
            let first = args.get_item(0)?;
            if let Ok(security_type) = security_type_from_py(&first) {
                let name = args.get_item(1)?.extract::<String>()?;
                let resolution = args.get_item(2)?.extract::<PyResolution>()?;
                let market = Market::new(args.get_item(3)?.extract::<String>()?);
                let selector_index = if args.len() >= 6 { 5 } else { 4 };
                let selector = args.get_item(selector_index)?.unbind();
                let universe = PyScheduledUniverse::user_defined_typed(
                    selector,
                    resolution.into(),
                    self.universe_settings.snapshot(),
                    security_type,
                    market,
                );
                self.universes.lock().unwrap().push(Py::new(py, universe)?);
                let _ = name;
                return Ok(());
            }
        }

        if args.len() >= 4 {
            let source_type = args.get_item(0)?.extract::<String>()?;
            let ticker = args.get_item(1)?.extract::<String>()?;
            let resolution = args.get_item(2)?.extract::<PyResolution>()?;
            let selector = args.get_item(3)?.unbind();
            self.register_custom_universe_subscription(
                &source_type,
                &ticker,
                resolution.into(),
                HashMap::new(),
            );
            let universe = PyScheduledUniverse::custom_data(
                source_type,
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
            "add_universe expects ScheduledUniverse, (name, resolution, selector), (source, name, resolution, selector), or (security_type, name, resolution, market, selector)",
        ))
    }

    #[pyo3(signature = (universe, resolution, selector, market=None))]
    fn add_crypto_universe(
        &mut self,
        py: Python<'_>,
        universe: &str,
        resolution: PyResolution,
        selector: Py<PyAny>,
        market: Option<&str>,
    ) -> PyResult<()> {
        let universe = normalize_hyperliquid_universe(universe);
        let market = Market::new(market.unwrap_or(Market::HYPERLIQUID));
        let security_type = hyperliquid_universe_security_type(&universe);
        let mut properties = HashMap::new();
        properties.insert("universe".to_string(), universe.clone());
        properties.insert("market".to_string(), market.as_str().to_string());
        properties.insert("security_type".to_string(), security_type.to_string());
        let universe_properties = properties.clone();
        self.register_custom_universe_subscription(
            "hyperliquid",
            &universe,
            resolution.into(),
            properties,
        );
        let universe = PyScheduledUniverse::custom_data_typed(
            "hyperliquid".to_string(),
            universe,
            selector,
            resolution.into(),
            self.universe_settings.snapshot(),
            security_type,
            market,
        )
        .with_custom_properties(universe_properties);
        self.universes.lock().unwrap().push(Py::new(py, universe)?);
        Ok(())
    }

    #[pyo3(name = "AddCryptoUniverse", signature = (universe, resolution, selector, market=None))]
    fn add_crypto_universe_pascal(
        &mut self,
        py: Python<'_>,
        universe: &str,
        resolution: PyResolution,
        selector: Py<PyAny>,
        market: Option<&str>,
    ) -> PyResult<()> {
        self.add_crypto_universe(py, universe, resolution, selector, market)
    }

    #[pyo3(signature = (universe, resolution, selector))]
    fn add_hyperliquid_universe(
        &mut self,
        py: Python<'_>,
        universe: &str,
        resolution: PyResolution,
        selector: Py<PyAny>,
    ) -> PyResult<()> {
        self.add_crypto_universe(
            py,
            universe,
            resolution,
            selector,
            Some(Market::HYPERLIQUID),
        )
    }

    // ─── Ordering ─────────────────────────────────────────────────────────────

    /// LEAN API: place a market order.
    #[pyo3(signature = (symbol, quantity, time_in_force=None, outside_regular_trading_hours=false))]
    fn market_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        time_in_force: Option<&Bound<'_, PyAny>>,
        outside_regular_trading_hours: bool,
    ) -> PyResult<PyOrderTicket> {
        let sym = self.resolve_symbol(symbol)?;
        let time_in_force = py_time_in_force(time_in_force)?;
        let ticket = self.inner.lock().unwrap().market_order_with_options(
            &sym,
            f2d(quantity),
            time_in_force,
            outside_regular_trading_hours,
        );
        Ok(PyOrderTicket::new(ticket, self.inner.clone()))
    }

    /// LEAN API: `self.buy(symbol, quantity)` — market buy.
    #[pyo3(signature = (symbol, quantity, time_in_force=None, outside_regular_trading_hours=false))]
    fn buy(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        time_in_force: Option<&Bound<'_, PyAny>>,
        outside_regular_trading_hours: bool,
    ) -> PyResult<PyOrderTicket> {
        self.market_order(
            symbol,
            quantity.abs(),
            time_in_force,
            outside_regular_trading_hours,
        )
    }

    /// LEAN API: `self.sell(symbol, quantity)` — market sell.
    #[pyo3(signature = (symbol, quantity, time_in_force=None, outside_regular_trading_hours=false))]
    fn sell(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        time_in_force: Option<&Bound<'_, PyAny>>,
        outside_regular_trading_hours: bool,
    ) -> PyResult<PyOrderTicket> {
        self.market_order(
            symbol,
            -quantity.abs(),
            time_in_force,
            outside_regular_trading_hours,
        )
    }

    /// LEAN API: `self.order(symbol, quantity)` — alias for market_order.
    #[pyo3(signature = (symbol, quantity, time_in_force=None, outside_regular_trading_hours=false))]
    fn order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        time_in_force: Option<&Bound<'_, PyAny>>,
        outside_regular_trading_hours: bool,
    ) -> PyResult<PyOrderTicket> {
        self.market_order(
            symbol,
            quantity,
            time_in_force,
            outside_regular_trading_hours,
        )
    }

    /// Place a limit order.
    #[pyo3(signature = (symbol, quantity, limit_price, time_in_force=None, outside_regular_trading_hours=false, post_only=false))]
    fn limit_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        limit_price: f64,
        time_in_force: Option<&Bound<'_, PyAny>>,
        outside_regular_trading_hours: bool,
        post_only: bool,
    ) -> PyResult<PyOrderTicket> {
        let sym = self.resolve_symbol(symbol)?;
        let time_in_force = py_time_in_force(time_in_force)?;
        let ticket = self.inner.lock().unwrap().limit_order_with_properties(
            &sym,
            f2d(quantity),
            f2d(limit_price),
            time_in_force,
            outside_regular_trading_hours,
            post_only,
        );
        Ok(PyOrderTicket::new(ticket, self.inner.clone()))
    }

    /// Place a stop-market order.
    #[pyo3(signature = (symbol, quantity, stop_price, time_in_force=None, outside_regular_trading_hours=false))]
    fn stop_market_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
        stop_price: f64,
        time_in_force: Option<&Bound<'_, PyAny>>,
        outside_regular_trading_hours: bool,
    ) -> PyResult<PyOrderTicket> {
        let sym = self.resolve_symbol(symbol)?;
        let time_in_force = py_time_in_force(time_in_force)?;
        let ticket = self.inner.lock().unwrap().stop_market_order_with_options(
            &sym,
            f2d(quantity),
            f2d(stop_price),
            time_in_force,
            outside_regular_trading_hours,
        );
        Ok(PyOrderTicket::new(ticket, self.inner.clone()))
    }

    /// Place a market-on-open order.
    #[pyo3(signature = (symbol, quantity))]
    fn market_on_open_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
    ) -> PyResult<PyOrderTicket> {
        let sym = self.resolve_symbol(symbol)?;
        let ticket = self
            .inner
            .lock()
            .unwrap()
            .market_on_open_order(&sym, f2d(quantity));
        Ok(PyOrderTicket::new(ticket, self.inner.clone()))
    }

    /// Place a market-on-close order.
    #[pyo3(signature = (symbol, quantity))]
    fn market_on_close_order(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        quantity: f64,
    ) -> PyResult<PyOrderTicket> {
        let sym = self.resolve_symbol(symbol)?;
        let ticket = self
            .inner
            .lock()
            .unwrap()
            .market_on_close_order(&sym, f2d(quantity));
        Ok(PyOrderTicket::new(ticket, self.inner.clone()))
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
        self.register_custom_data_subscription(
            source_type,
            ticker,
            res,
            properties,
            lean_data::CustomDataSubscriptionRole::Data,
        );

        // Return a synthetic security object so callers can do:
        //   self.unrate = self.add_data("fred", "UNRATE").symbol
        let market = lean_core::Market::usa();
        let sym = lean_core::Symbol::create_equity(ticker, &market);
        self.symbols
            .lock()
            .unwrap()
            .insert(ticker.to_uppercase(), sym.clone());
        Ok(PySecurity::from_algorithm_symbol(
            PySymbol { inner: sym },
            self.inner.clone(),
        ))
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

    // ─── History ─────────────────────────────────────────────────────────────

    /// LEAN-style algorithm history.
    ///
    /// Supported overloads:
    ///   self.history(symbol, bar_count, resolution)
    ///   self.history(symbol, start, end, resolution)
    ///
    /// Optional keyword `data_normalization_mode` overrides the subscription
    /// config's normalization mode, mirroring C# Lean's
    /// `History(..., dataNormalizationMode: ...)` overloads.
    ///
    /// Returns a column-oriented dict suitable for pandas.DataFrame(...).
    #[pyo3(signature = (*args, data_normalization_mode=None))]
    fn history(
        &self,
        py: Python<'_>,
        args: &Bound<'_, PyTuple>,
        data_normalization_mode: Option<PyDataNormalizationMode>,
    ) -> PyResult<Py<PyAny>> {
        if args.len() != 3 && args.len() != 4 {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "history expects (symbol, bar_count, resolution) or (symbol, start, end, resolution)",
            ));
        }

        let symbol_arg = args.get_item(0)?;
        let resolution_arg = args.get_item(args.len() - 1)?;
        let resolution: Resolution = resolution_arg.extract::<PyResolution>()?.into();

        let (start, end, bar_count) = if args.len() == 3 {
            let bar_count = args.get_item(1)?.extract::<usize>()?;
            let end = self.history_end_date();
            let calendar_days = (bar_count as i64 * 7 + 4) / 5 + 10;
            (
                end - chrono::Duration::days(calendar_days),
                end,
                Some(bar_count),
            )
        } else {
            (
                parse_history_date(&args.get_item(1)?)?,
                parse_history_date(&args.get_item(2)?)?,
                None,
            )
        };

        let ticker = history_ticker(&symbol_arg)?;
        if let Some(custom_sub) = self.find_custom_subscription(&ticker) {
            let mut points = self.load_custom_history(&custom_sub, start, end)?;
            points.sort_by_key(|p| {
                p.end_time
                    .map(|t| t.0)
                    .unwrap_or_else(|| date_to_datetime(p.time, 0, 0, 0).0)
            });
            if let Some(bar_count) = bar_count {
                points = filter_custom_points_by_last_dates(points, bar_count);
            }
            return custom_points_to_pydict(py, &points);
        }

        let symbol = self.resolve_symbol(&symbol_arg)?;
        let mode_override = data_normalization_mode.map(Into::into);
        let mut bars = self.load_history_bars(&symbol, resolution, start, end, mode_override)?;
        bars.sort_by_key(|b| b.time.0);
        if let Some(bar_count) = bar_count {
            if bars.len() > bar_count {
                bars = bars[bars.len() - bar_count..].to_vec();
            }
        }
        trade_bars_to_pydict(py, &bars)
    }

    fn get_last_known_prices(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let symbol = self.resolve_symbol(symbol)?;
        let bars = self.load_last_known_trade_bars(&symbol)?;
        let list = PyList::empty(py);
        for bar in bars {
            list.append(Py::new(py, PyTradeBar::from(&bar))?)?;
        }
        Ok(list.into())
    }

    fn get_last_known_price(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let symbol = self.resolve_symbol(symbol)?;
        let bars = self.load_last_known_trade_bars(&symbol)?;
        if let Some(bar) = bars.last() {
            return Ok(Py::new(py, PyTradeBar::from(bar))?.into_any());
        }
        Ok(py.None())
    }

    /// Explicit date-range alias for the smaller rlean API.
    #[pyo3(signature = (symbol, start, end, resolution))]
    fn history_range(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
        start: &Bound<'_, PyAny>,
        end: &Bound<'_, PyAny>,
        resolution: PyResolution,
    ) -> PyResult<Py<PyAny>> {
        let start = parse_history_date(start)?;
        let end = parse_history_date(end)?;
        let ticker = history_ticker(symbol)?;
        if let Some(custom_sub) = self.find_custom_subscription(&ticker) {
            let mut points = self.load_custom_history(&custom_sub, start, end)?;
            points.sort_by_key(|p| {
                p.end_time
                    .map(|t| t.0)
                    .unwrap_or_else(|| date_to_datetime(p.time, 0, 0, 0).0)
            });
            return custom_points_to_pydict(py, &points);
        }

        let symbol = self.resolve_symbol(symbol)?;
        let mut bars = self.load_history_bars(&symbol, resolution.into(), start, end, None)?;
        bars.sort_by_key(|b| b.time.0);
        trade_bars_to_pydict(py, &bars)
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
            algorithm: self.inner.clone(),
        })
    }

    /// Subscribe to a specific option or index-option contract.
    #[pyo3(signature = (symbol, resolution=None))]
    fn add_option_contract(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        resolution: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<crate::py_types::PySymbol> {
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
        let symbol = self.resolve_symbol(symbol)?;
        if !symbol.security_type().is_option_like() {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "add_option_contract requires an option or index-option Symbol",
            ));
        }
        let contract = self.inner.lock().unwrap().add_option_contract(symbol, res);
        Ok(crate::py_types::PySymbol { inner: contract })
    }

    /// LEAN API: remove a security subscription.
    ///
    /// Mirrors C# LEAN's `RemoveSecurity(symbol, tag=None)` surface. For a
    /// canonical option symbol, this unsubscribes the option universe/filter.
    #[pyo3(signature = (symbol, tag=None))]
    fn remove_security(&mut self, symbol: &Bound<'_, PyAny>, tag: Option<&str>) -> PyResult<bool> {
        let sym = self.resolve_symbol(symbol)?;
        Ok(self.inner.lock().unwrap().remove_security(&sym, tag))
    }

    /// LEAN API sugar for removing a specific option contract.
    #[pyo3(signature = (symbol, tag=None))]
    fn remove_option_contract(
        &mut self,
        symbol: &Bound<'_, PyAny>,
        tag: Option<&str>,
    ) -> PyResult<bool> {
        let sym = self.resolve_symbol(symbol)?;
        Ok(self.inner.lock().unwrap().remove_option_contract(&sym, tag))
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
                PySecurityManager::build_entry(sec.symbol.clone(), self.inner.clone()),
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

    /// rlean framework helper: Black-Scholes implied-volatility inversion for
    /// selected option prices. This keeps pricing math in Rust while allowing
    /// strategies to decide which suspicious events deserve an IV calculation.
    #[pyo3(signature = (contract, option_price, underlying_price=None, risk_free_rate=0.0, dividend_yield=0.0))]
    fn calculate_implied_volatility(
        &self,
        contract: &crate::py_options::PyOptionContract,
        option_price: f64,
        underlying_price: Option<f64>,
        risk_free_rate: f64,
        dividend_yield: f64,
    ) -> Option<f64> {
        let spot = underlying_price.unwrap_or_else(|| {
            contract
                .inner
                .data
                .underlying_last_price
                .to_f64()
                .unwrap_or(0.0)
        });
        let strike = contract.inner.strike.to_f64().unwrap_or(0.0);
        let valuation_time = self.inner.lock().unwrap().time;
        let t = time_to_expiry_years(contract.inner.expiry, valuation_time);
        if option_price <= 0.0 || spot <= 0.0 || strike <= 0.0 || t <= 0.0 {
            return None;
        }
        let iv = implied_volatility(
            option_price,
            spot,
            strike,
            t,
            risk_free_rate,
            dividend_yield,
            contract.inner.right,
        );
        if iv.is_finite() && iv > 0.0 {
            Some(iv)
        } else {
            None
        }
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
        {
            let mut g = fw.lock().unwrap();
            if g.alg_py.is_none() {
                g.alg_py = Some(slf.clone().into_any().unbind());
            }
        }
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
        {
            let mut g = fw.lock().unwrap();
            g.alg_py = Some(slf.clone().into_any().unbind());
        }
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

pub(crate) fn ns_to_py_datetime(ns: i64) -> PyResult<Py<PyAny>> {
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

fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap_or_default()))
}

fn parse_history_date(value: &Bound<'_, PyAny>) -> PyResult<NaiveDate> {
    if let Ok((y, m, d)) = value.extract::<(i32, u32, u32)>() {
        return NaiveDate::from_ymd_opt(y, m, d).ok_or_else(|| {
            pyo3::exceptions::PyValueError::new_err(format!("invalid history date: {y}-{m}-{d}"))
        });
    }
    if let Ok(date) = value.extract::<NaiveDate>() {
        return Ok(date);
    }
    if let Ok(dt) = value.extract::<chrono::NaiveDateTime>() {
        return Ok(dt.date());
    }
    if let Ok(s) = value.extract::<String>() {
        return NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "invalid history date string '{s}', expected YYYY-MM-DD"
            ))
        });
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "history dates must be datetime/date, (year, month, day), or YYYY-MM-DD",
    ))
}

fn history_ticker(value: &Bound<'_, PyAny>) -> PyResult<String> {
    if let Ok(sym) = value.cast::<PySymbol>() {
        return Ok(sym.get().inner.permtick.to_uppercase());
    }
    if let Ok(sec) = value.cast::<PySecurity>() {
        return Ok(sec.get().inner.inner.permtick.to_uppercase());
    }
    if let Ok(ticker) = value.extract::<String>() {
        return Ok(ticker.to_uppercase());
    }
    Ok(value.str()?.to_str()?.to_uppercase())
}

fn trade_bars_to_pydict(py: Python<'_>, bars: &[TradeBar]) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item(
        "time",
        bars.iter()
            .map(|b| lean_datetime_to_date(b.time.0))
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "open",
        bars.iter()
            .map(|b| b.open.to_f64().unwrap_or(0.0))
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "high",
        bars.iter()
            .map(|b| b.high.to_f64().unwrap_or(0.0))
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "low",
        bars.iter()
            .map(|b| b.low.to_f64().unwrap_or(0.0))
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "close",
        bars.iter()
            .map(|b| b.close.to_f64().unwrap_or(0.0))
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "volume",
        bars.iter()
            .map(|b| b.volume.to_f64().unwrap_or(0.0))
            .collect::<Vec<_>>(),
    )?;
    Ok(dict.into())
}

fn custom_points_to_pydict(py: Python<'_>, points: &[CustomDataPoint]) -> PyResult<Py<PyAny>> {
    let dict = PyDict::new(py);
    dict.set_item(
        "time",
        points
            .iter()
            .map(|p| p.time.to_string())
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "end_time",
        points
            .iter()
            .map(|p| {
                p.end_time
                    .map(|t| lean_datetime_to_iso(t.0))
                    .unwrap_or_else(|| p.time.to_string())
            })
            .collect::<Vec<_>>(),
    )?;
    dict.set_item(
        "value",
        points
            .iter()
            .map(|p| p.value.to_f64().unwrap_or(0.0))
            .collect::<Vec<_>>(),
    )?;

    let mut field_names: Vec<String> = points
        .iter()
        .flat_map(|p| p.fields.keys().cloned())
        .collect();
    field_names.sort();
    field_names.dedup();
    for field in field_names {
        let values = PyList::empty(py);
        for point in points {
            match point.fields.get(&field) {
                Some(value) => values.append(json_value_to_py_history(py, value)?)?,
                None => values.append(py.None())?,
            }
        }
        dict.set_item(field, values)?;
    }
    Ok(dict.into())
}

fn filter_custom_points_by_last_dates(
    points: Vec<CustomDataPoint>,
    bar_count: usize,
) -> Vec<CustomDataPoint> {
    if bar_count == 0 || points.is_empty() {
        return Vec::new();
    }

    let mut dates: Vec<NaiveDate> = points
        .iter()
        .map(|p| p.end_time.map(|t| t.date_utc()).unwrap_or(p.time))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if dates.len() <= bar_count {
        return points;
    }

    dates = dates.split_off(dates.len() - bar_count);
    let keep: BTreeSet<NaiveDate> = dates.into_iter().collect();
    points
        .into_iter()
        .filter(|p| keep.contains(&p.end_time.map(|t| t.date_utc()).unwrap_or(p.time)))
        .collect()
}

fn json_value_to_py_history(py: Python<'_>, v: &serde_json::Value) -> PyResult<Py<PyAny>> {
    use pyo3::IntoPyObjectExt;
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
        other => other.to_string().into_py_any(py),
    }
}

impl PyQcAlgorithm {
    fn history_context(&self) -> PyResult<AlgorithmHistoryContext> {
        self.history_context.lock().unwrap().clone().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err(
                "history is only available while running under the rlean backtest runner",
            )
        })
    }

    fn history_service(&self) -> PyResult<HistoryService> {
        Ok(HistoryService::new(self.history_context()?))
    }

    fn history_end_date(&self) -> NaiveDate {
        let inner = self.inner.lock().unwrap();
        let current = inner.time.date_utc();
        if current == DateTime::EPOCH.date_utc() {
            inner.start_date.date_utc()
        } else {
            current
        }
    }

    fn find_custom_subscription(&self, ticker: &str) -> Option<CustomDataSubscription> {
        let inner = self.inner.lock().unwrap();
        inner
            .custom_data_subscriptions
            .iter()
            .find(|sub| sub.ticker.eq_ignore_ascii_case(ticker))
            .cloned()
    }

    fn load_history_bars(
        &self,
        symbol: &lean_core::Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
        normalization_mode_override: Option<DataNormalizationMode>,
    ) -> PyResult<Vec<TradeBar>> {
        let normalization_mode = normalization_mode_override
            .unwrap_or_else(|| self.matching_normalization_mode(symbol, Some(resolution)));
        self.history_service()?
            .load_trade_bars_blocking_with_normalization(
                symbol,
                resolution,
                start,
                end,
                normalization_mode,
            )
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    fn load_last_known_trade_bars(&self, symbol: &lean_core::Symbol) -> PyResult<Vec<TradeBar>> {
        let (resolution, end) = {
            let inner = self.inner.lock().unwrap();
            let resolution = inner
                .subscription_manager
                .get_all()
                .into_iter()
                .find(|sub| sub.symbol.id.sid == symbol.id.sid && sub.tick_type == TickType::Trade)
                .or_else(|| {
                    inner
                        .subscription_manager
                        .get_all()
                        .into_iter()
                        .find(|sub| sub.symbol.id.sid == symbol.id.sid)
                })
                .map(|sub| sub.resolution)
                .unwrap_or(Resolution::Minute);
            let end = if inner.time == DateTime::EPOCH {
                inner.start_date
            } else {
                inner.time
            };
            (resolution, end)
        };

        let lookback_days = match resolution {
            Resolution::Tick | Resolution::Second | Resolution::Minute => 7,
            Resolution::Hour => 14,
            Resolution::Daily => 31,
        };
        let end_date = end.date_utc();
        let start_date = end_date - chrono::Duration::days(lookback_days);
        let start = DateTime::from(
            chrono::Utc.from_utc_datetime(&start_date.and_hms_opt(0, 0, 0).unwrap()),
        );
        let mut bars = self
            .history_service()?
            .load_trade_bars_between_blocking_with_normalization(
                symbol,
                resolution,
                start,
                end,
                self.matching_normalization_mode(symbol, Some(resolution)),
            )
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        bars.retain(|bar| bar.close > Decimal::ZERO && bar.end_time.0 <= end.0);
        bars.sort_by_key(|bar| bar.end_time.0);
        if bars.len() > 1 {
            bars = bars[bars.len() - 1..].to_vec();
        }
        Ok(bars)
    }

    /// Mirror of C# Lean's `QCAlgorithm.GetMatchingSubscriptions` for the
    /// narrow purpose of picking a normalization mode for a history request:
    /// prefer a trade-tick subscription at the requested resolution, then any
    /// subscription for the symbol, and finally fall back to the configured
    /// `UniverseSettings.DataNormalizationMode` (not a security-type default).
    fn matching_normalization_mode(
        &self,
        symbol: &lean_core::Symbol,
        resolution: Option<Resolution>,
    ) -> DataNormalizationMode {
        let inner = self.inner.lock().unwrap();
        let configs = inner.subscription_manager.get_configs_for_symbol(symbol);
        if let Some(resolution) = resolution {
            if let Some(sub) = configs
                .iter()
                .find(|sub| sub.resolution == resolution && sub.tick_type == TickType::Trade)
            {
                return sub.normalization_mode;
            }
            if let Some(sub) = configs.iter().find(|sub| sub.resolution == resolution) {
                return sub.normalization_mode;
            }
        }
        if let Some(sub) = configs.iter().find(|sub| sub.tick_type == TickType::Trade) {
            return sub.normalization_mode;
        }
        if let Some(sub) = configs.first() {
            return sub.normalization_mode;
        }
        drop(inner);
        self.universe_settings.snapshot().data_normalization_mode
    }

    fn load_custom_history(
        &self,
        sub: &CustomDataSubscription,
        start: NaiveDate,
        end: NaiveDate,
    ) -> PyResult<Vec<CustomDataPoint>> {
        self.history_service()?
            .load_custom_history_blocking(sub, start, end)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))
    }

    fn resolve_symbol(&self, arg: &Bound<'_, PyAny>) -> PyResult<lean_core::Symbol> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(sym.get().inner.clone());
        }
        // Accept Security objects directly (mirrors LEAN's set_holdings(security, ...) API)
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(sec.get().inner.inner.clone());
        }
        // Accept Option security objects returned by add_option().
        if let Ok(option) = arg.cast::<PyOptionSecurity>() {
            return Ok(option.borrow().canonical.inner.clone());
        }
        // Accept OptionContract objects — uses contract.symbol
        if let Ok(contract) = arg.cast::<crate::py_options::PyOptionContract>() {
            return Ok(contract.borrow().inner.symbol.clone());
        }
        if let Ok(ticker) = arg.extract::<String>() {
            let upper = ticker.to_uppercase();
            if let Some(sym) = self.symbols.lock().unwrap().get(&upper) {
                return Ok(sym.clone());
            }
            // Fall back to creating a new US equity symbol
            return Ok(lean_core::Symbol::create_equity(&ticker, &Market::usa()));
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, OptionContract, or ticker string",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::TickType;
    use lean_data::TradeBarData;
    use lean_data_providers::{LocalHistoryProvider, StackedHistoryProvider};
    use lean_storage::{
        custom_data_path, FactorFileEntry, ParquetWriter, PathResolver, WriterConfig,
    };
    use rust_decimal_macros::dec;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct StaticHistoryProvider {
        bars: Vec<TradeBar>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl lean_data_providers::IHistoryProvider for StaticHistoryProvider {
        async fn get_history(
            &self,
            request: &lean_data_providers::HistoryRequest,
        ) -> anyhow::Result<Vec<TradeBar>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(request.resolution, Resolution::Daily);
            assert_eq!(request.data_type, lean_data_providers::DataType::TradeBar);
            Ok(self.bars.clone())
        }
    }

    #[test]
    fn add_crypto_universe_keeps_universe_settings_resolution_for_selected_symbols() {
        Python::initialize();
        Python::attach(|py| {
            let mut alg = PyQcAlgorithm::new();
            alg.universe_settings.inner.lock().unwrap().resolution = Resolution::Minute;
            let selector = py
                .eval(c"lambda rows: ['BTC']", None, None)
                .unwrap()
                .unbind();

            alg.add_crypto_universe(py, "HIP3_XYZ", PyResolution::Hour, selector, None)
                .unwrap();

            let universes = alg.universes.lock().unwrap();
            assert_eq!(universes.len(), 1);
            let universe = universes[0].bind(py).borrow();
            assert_eq!(universe.settings().resolution, Resolution::Minute);
            assert_eq!(
                universe.live_universe_subscription().unwrap().resolution,
                Resolution::Hour
            );
        });
    }

    #[test]
    fn add_equity_passes_data_normalization_mode_into_subscription_config() {
        Python::initialize();
        Python::attach(|py| {
            let alg = PyQcAlgorithm::new();
            let security = alg
                .add_equity(
                    py,
                    "SPY",
                    PyResolution::Daily,
                    None,
                    true,
                    None,
                    false,
                    Some(PyDataNormalizationMode::Raw),
                    None,
                )
                .unwrap();
            let symbol = security.inner.inner.clone();
            let inner = alg.inner.lock().unwrap();
            let configs = inner.subscription_manager.get_configs_for_symbol(&symbol);
            assert_eq!(configs.len(), 1);
            assert_eq!(configs[0].normalization_mode, DataNormalizationMode::Raw);
        });
    }

    #[test]
    fn add_equity_uses_universe_settings_data_normalization_mode() {
        Python::initialize();
        Python::attach(|py| {
            let alg = PyQcAlgorithm::new();
            alg.universe_settings
                .inner
                .lock()
                .unwrap()
                .data_normalization_mode = DataNormalizationMode::Raw;
            let security = alg
                .add_equity(
                    py,
                    "SPY",
                    PyResolution::Daily,
                    None,
                    true,
                    None,
                    false,
                    None,
                    None,
                )
                .unwrap();
            let symbol = security.inner.inner.clone();
            let inner = alg.inner.lock().unwrap();
            let configs = inner.subscription_manager.get_configs_for_symbol(&symbol);
            assert_eq!(configs.len(), 1);
            assert_eq!(configs[0].normalization_mode, DataNormalizationMode::Raw);
        });
    }

    #[test]
    fn algorithm_history_range_reads_equity_fixture_rows() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let alg = PyQcAlgorithm::new();
        let security = Python::attach(|py| {
            alg.add_equity(
                py,
                "SPY",
                PyResolution::Daily,
                None,
                true,
                None,
                false,
                Some(PyDataNormalizationMode::Adjusted),
                None,
            )
            .unwrap()
        });
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: None,
            custom_data_sources: Vec::new(),
        });

        let symbol = security.inner.inner.clone();
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100.5), dec!(1000)),
            ),
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(101), dec!(102), dec!(100), dec!(101.5), dec!(2000)),
            ),
        ];
        let writer = ParquetWriter::new(WriterConfig::default());
        for bar in &bars {
            let path = resolver.market_data_partition(
                &symbol,
                Resolution::Daily,
                TickType::Trade,
                bar.time.date_utc(),
            );
            writer
                .write_trade_bars(std::slice::from_ref(bar), &path)
                .unwrap();
        }

        Python::attach(|py| {
            let py_symbol = Py::new(py, PySymbol { inner: symbol }).unwrap();
            let start = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let end = PyTuple::new(py, [2024, 1, 3]).unwrap();
            let result = alg
                .history_range(
                    py,
                    py_symbol.bind(py).as_any(),
                    start.as_any(),
                    end.as_any(),
                    PyResolution::Daily,
                )
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let closes: Vec<f64> = dict.get_item("close").unwrap().unwrap().extract().unwrap();
            assert_eq!(closes, vec![100.5, 101.5]);
        });
    }

    #[test]
    fn algorithm_history_range_uses_configured_history_provider() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let alg = PyQcAlgorithm::new();
        let security = Python::attach(|py| {
            alg.add_equity(
                py,
                "SPY",
                PyResolution::Daily,
                None,
                true,
                None,
                false,
                Some(PyDataNormalizationMode::Adjusted),
                None,
            )
            .unwrap()
        });
        let symbol = security.inner.inner.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100.5), dec!(1000)),
            ),
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(101), dec!(102), dec!(100), dec!(101.5), dec!(2000)),
            ),
        ];
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: Some(Arc::new(StaticHistoryProvider {
                bars,
                calls: Arc::clone(&calls),
            })),
            custom_data_sources: Vec::new(),
        });

        Python::attach(|py| {
            let py_symbol = Py::new(py, PySymbol { inner: symbol }).unwrap();
            let start = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let end = PyTuple::new(py, [2024, 1, 3]).unwrap();
            let result = alg
                .history_range(
                    py,
                    py_symbol.bind(py).as_any(),
                    start.as_any(),
                    end.as_any(),
                    PyResolution::Daily,
                )
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let closes: Vec<f64> = dict.get_item("close").unwrap().unwrap().extract().unwrap();
            assert_eq!(closes, vec![100.5, 101.5]);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn algorithm_history_range_applies_adjusted_subscription_normalization() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let alg = PyQcAlgorithm::new();
        let security = Python::attach(|py| {
            alg.add_equity(
                py,
                "SPY",
                PyResolution::Daily,
                None,
                true,
                None,
                false,
                None,
                None,
            )
            .unwrap()
        });
        let symbol = security.inner.inner.clone();
        let date = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let writer = ParquetWriter::new(WriterConfig::default());

        let bar = TradeBar::new(
            symbol.clone(),
            date_to_datetime(date, 16, 0, 0),
            lean_core::TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        writer
            .write_trade_bars(
                std::slice::from_ref(&bar),
                &resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, date),
            )
            .unwrap();
        writer
            .write_factor_file(
                &[FactorFileEntry {
                    date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                    price_factor: 0.5,
                    split_factor: 1.0,
                    reference_price: 0.0,
                }],
                &resolver.factor_file("usa", "SPY"),
            )
            .unwrap();
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: None,
            custom_data_sources: Vec::new(),
        });

        Python::attach(|py| {
            let py_symbol = Py::new(py, PySymbol { inner: symbol }).unwrap();
            let start = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let end = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let result = alg
                .history_range(
                    py,
                    py_symbol.bind(py).as_any(),
                    start.as_any(),
                    end.as_any(),
                    PyResolution::Daily,
                )
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let closes: Vec<f64> = dict.get_item("close").unwrap().unwrap().extract().unwrap();
            assert_eq!(closes, vec![50.0]);
        });
    }

    /// Set up SPY daily with a factor file that halves the bar so we can
    /// distinguish Adjusted (50.0) from Raw (100.0) without re-writing fixtures.
    fn write_normalization_fixture(tmp: &tempfile::TempDir) -> (PyQcAlgorithm, lean_core::Symbol) {
        let resolver = PathResolver::new(tmp.path());
        let alg = PyQcAlgorithm::new();
        let security = Python::attach(|py| {
            alg.add_equity(
                py,
                "SPY",
                PyResolution::Daily,
                None,
                true,
                None,
                false,
                None,
                None,
            )
            .unwrap()
        });
        let symbol = security.inner.inner.clone();
        let date = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let writer = ParquetWriter::new(WriterConfig::default());
        let bar = TradeBar::new(
            symbol.clone(),
            date_to_datetime(date, 16, 0, 0),
            lean_core::TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        writer
            .write_trade_bars(
                std::slice::from_ref(&bar),
                &resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, date),
            )
            .unwrap();
        writer
            .write_factor_file(
                &[FactorFileEntry {
                    date: NaiveDate::from_ymd_opt(2020, 1, 1).unwrap(),
                    price_factor: 0.5,
                    split_factor: 1.0,
                    reference_price: 0.0,
                }],
                &resolver.factor_file("usa", "SPY"),
            )
            .unwrap();
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: None,
            custom_data_sources: Vec::new(),
        });
        (alg, symbol)
    }

    #[test]
    fn set_data_normalization_mode_raw_returns_unadjusted_history() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let (alg, symbol) = write_normalization_fixture(&tmp);

        alg.inner
            .lock()
            .unwrap()
            .set_data_normalization_mode(&symbol, DataNormalizationMode::Raw);

        Python::attach(|py| {
            let py_symbol = Py::new(py, PySymbol { inner: symbol }).unwrap();
            let start = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let end = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let result = alg
                .history_range(
                    py,
                    py_symbol.bind(py).as_any(),
                    start.as_any(),
                    end.as_any(),
                    PyResolution::Daily,
                )
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let closes: Vec<f64> = dict.get_item("close").unwrap().unwrap().extract().unwrap();
            assert_eq!(closes, vec![100.0]);
        });
    }

    #[test]
    fn history_kwarg_overrides_subscription_normalization_mode() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let (alg, symbol) = write_normalization_fixture(&tmp);

        Python::attach(|py| {
            let py_symbol = Py::new(
                py,
                PySymbol {
                    inner: symbol.clone(),
                },
            )
            .unwrap();
            let args = PyTuple::new(
                py,
                [
                    py_symbol.bind(py).as_any(),
                    PyTuple::new(py, [2024, 1, 2]).unwrap().as_any(),
                    PyTuple::new(py, [2024, 1, 2]).unwrap().as_any(),
                    Py::new(py, PyResolution::Daily).unwrap().bind(py).as_any(),
                ],
            )
            .unwrap();
            // Override Adjusted (subscription default) with Raw.
            let result = alg
                .history(py, &args, Some(PyDataNormalizationMode::Raw))
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let closes: Vec<f64> = dict.get_item("close").unwrap().unwrap().extract().unwrap();
            assert_eq!(closes, vec![100.0]);
        });
    }

    #[test]
    fn matching_normalization_mode_falls_back_to_universe_settings_when_unsubscribed() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let alg = PyQcAlgorithm::new();
        alg.universe_settings
            .inner
            .lock()
            .unwrap()
            .data_normalization_mode = DataNormalizationMode::Raw;
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: None,
            custom_data_sources: Vec::new(),
        });
        let unsubscribed = lean_core::Symbol::create_equity("AAPL", &Market::usa());
        assert_eq!(
            alg.matching_normalization_mode(&unsubscribed, Some(Resolution::Daily)),
            DataNormalizationMode::Raw,
        );
    }

    #[test]
    fn algorithm_history_range_prefers_local_cache_before_provider() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let alg = PyQcAlgorithm::new();
        let security = Python::attach(|py| {
            alg.add_equity(
                py,
                "SPY",
                PyResolution::Daily,
                None,
                true,
                None,
                false,
                None,
                None,
            )
            .unwrap()
        });
        let symbol = security.inner.inner.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let local_bars = vec![
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(200), dec!(201), dec!(199), dec!(200.5), dec!(1000)),
            ),
            TradeBar::new(
                symbol.clone(),
                date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(201), dec!(202), dec!(200), dec!(201.5), dec!(2000)),
            ),
        ];
        let provider_bars = vec![TradeBar::new(
            symbol.clone(),
            date_to_datetime(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), 16, 0, 0),
            lean_core::TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100.5), dec!(1000)),
        )];
        let writer = ParquetWriter::new(WriterConfig::default());
        for bar in &local_bars {
            let path = resolver.market_data_partition(
                &symbol,
                Resolution::Daily,
                TickType::Trade,
                bar.time.date_utc(),
            );
            writer
                .write_trade_bars(std::slice::from_ref(bar), &path)
                .unwrap();
        }
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: Some(Arc::new(StackedHistoryProvider::new(vec![
                Arc::new(LocalHistoryProvider::new(tmp.path())),
                Arc::new(StaticHistoryProvider {
                    bars: provider_bars,
                    calls: Arc::clone(&calls),
                }),
            ]))),
            custom_data_sources: Vec::new(),
        });

        Python::attach(|py| {
            let py_symbol = Py::new(py, PySymbol { inner: symbol }).unwrap();
            let start = PyTuple::new(py, [2024, 1, 2]).unwrap();
            let end = PyTuple::new(py, [2024, 1, 3]).unwrap();
            let result = alg
                .history_range(
                    py,
                    py_symbol.bind(py).as_any(),
                    start.as_any(),
                    end.as_any(),
                    PyResolution::Daily,
                )
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let closes: Vec<f64> = dict.get_item("close").unwrap().unwrap().extract().unwrap();
            assert_eq!(closes, vec![200.5, 201.5]);
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn security_initializer_seeds_new_equity_price() {
        Python::initialize();
        Python::attach(|py| {
            let mut alg = PyQcAlgorithm::new();
            let symbol = Symbol::create_equity("JOBY", &Market::usa());
            let bar = TradeBar::new(
                symbol,
                date_to_datetime(NaiveDate::from_ymd_opt(2026, 4, 1).unwrap(), 16, 0, 0),
                lean_core::TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(12), dec!(13), dec!(11), dec!(12.5), dec!(1000)),
            );
            let locals = PyDict::new(py);
            locals
                .set_item("bar", Py::new(py, PyTradeBar::from(&bar)).unwrap())
                .unwrap();
            let seed_function = py
                .eval(c"lambda security: [bar]", Some(&locals), Some(&locals))
                .unwrap();
            let seeder = Py::new(
                py,
                PyFuncSecuritySeeder::new(seed_function.unbind().into_any()),
            )
            .unwrap();
            let initializer = Py::new(
                py,
                PyBrokerageModelSecurityInitializer::new(py, None, Some(seeder.into_any())),
            )
            .unwrap();
            alg.set_security_initializer(initializer.into_any());

            let security = alg
                .add_equity(
                    py,
                    "JOBY",
                    PyResolution::Minute,
                    None,
                    true,
                    None,
                    false,
                    None,
                    None,
                )
                .unwrap();
            let price = alg
                .inner
                .lock()
                .unwrap()
                .securities
                .get(&security.inner.inner)
                .unwrap()
                .current_price();

            assert_eq!(price, Decimal::from_f64(12.5).unwrap());
        });
    }

    #[test]
    fn algorithm_history_range_reads_custom_subscription_cache_rows() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let mut alg = PyQcAlgorithm::new();
        alg.add_data("fixture", "ALT", None, None).unwrap();
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: None,
            custom_data_sources: Vec::new(),
        });

        let mut fields = HashMap::new();
        fields.insert("signal".to_string(), serde_json::json!("ready"));
        let points = vec![CustomDataPoint {
            time: NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
            end_time: Some(date_to_datetime(
                NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
                16,
                0,
                0,
            )),
            value: dec!(42),
            fields,
        }];
        let path = custom_data_path(
            tmp.path(),
            "fixture",
            "ALT",
            NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
        );
        ParquetWriter::new(WriterConfig {
            bloom_filter: false,
            write_statistics: false,
            ..WriterConfig::default()
        })
        .write_custom_data_points(&points, &path)
        .unwrap();

        Python::attach(|py| {
            let py_symbol = Py::new(
                py,
                PySymbol {
                    inner: alg.symbols.lock().unwrap().get("ALT").unwrap().clone(),
                },
            )
            .unwrap();
            let start = PyTuple::new(py, [2024, 1, 1]).unwrap();
            let end = PyTuple::new(py, [2024, 1, 4]).unwrap();
            let result = alg
                .history_range(
                    py,
                    py_symbol.bind(py).as_any(),
                    start.as_any(),
                    end.as_any(),
                    PyResolution::Daily,
                )
                .unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let values: Vec<f64> = dict.get_item("value").unwrap().unwrap().extract().unwrap();
            let signals: Vec<String> = dict.get_item("signal").unwrap().unwrap().extract().unwrap();
            assert_eq!(values, vec![42.0]);
            assert_eq!(signals, vec!["ready".to_string()]);
        });
    }

    #[test]
    fn algorithm_custom_history_bar_count_keeps_last_dates_not_rows() {
        Python::initialize();
        let tmp = tempfile::tempdir().unwrap();
        let mut alg = PyQcAlgorithm::new();
        alg.add_data("fixture", "ALT", None, None).unwrap();
        alg.set_history_context(AlgorithmHistoryContext {
            data_root: tmp.path().to_path_buf(),
            history_provider: None,
            custom_data_sources: Vec::new(),
        });

        for (day, values) in [(2, vec![1, 2]), (3, vec![3, 4]), (4, vec![5, 6])] {
            let date = NaiveDate::from_ymd_opt(2024, 1, day).unwrap();
            let points = values
                .into_iter()
                .map(|value| CustomDataPoint {
                    time: date,
                    end_time: Some(date_to_datetime(date, 16, 0, 0)),
                    value: Decimal::from(value),
                    fields: HashMap::new(),
                })
                .collect::<Vec<_>>();
            let path = custom_data_path(tmp.path(), "fixture", "ALT", date);
            ParquetWriter::new(WriterConfig {
                bloom_filter: false,
                write_statistics: false,
                ..WriterConfig::default()
            })
            .write_custom_data_points(&points, &path)
            .unwrap();
        }

        let custom_sub = alg.find_custom_subscription("ALT").unwrap();
        let mut points = alg
            .load_custom_history(
                &custom_sub,
                NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                NaiveDate::from_ymd_opt(2024, 1, 4).unwrap(),
            )
            .unwrap();
        points.sort_by_key(|p| {
            p.end_time
                .map(|t| t.0)
                .unwrap_or_else(|| date_to_datetime(p.time, 0, 0, 0).0)
        });
        let values = filter_custom_points_by_last_dates(points, 2)
            .into_iter()
            .map(|p| p.value.to_f64().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, vec![3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn market_on_open_order_is_exposed_to_python_algorithm() {
        Python::initialize();
        Python::attach(|py| {
            let mut alg = PyQcAlgorithm::new();
            let security = alg
                .add_equity(
                    py,
                    "SPY",
                    PyResolution::Daily,
                    None,
                    true,
                    None,
                    false,
                    None,
                    None,
                )
                .unwrap();
            let symbol = Py::new(py, security.inner.clone()).unwrap();

            let ticket = alg
                .market_on_open_order(symbol.bind(py).as_any(), 10.0)
                .unwrap();

            let order = alg
                .inner
                .lock()
                .unwrap()
                .transactions
                .get_order(ticket.order_id)
                .unwrap();
            assert_eq!(order.order_type, lean_orders::OrderType::MarketOnOpen);
            assert_eq!(order.quantity, dec!(10));
        });
    }
}
