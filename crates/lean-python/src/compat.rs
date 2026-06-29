use crate::sdk_bindings::{
    PyBar, PyExponentialMovingAverage, PyIndicatorDataPoint, PyInsight, PyInsightDirection,
    PyPortfolioTarget, PyQCAlgorithm, PyQuoteBar, PyRelativeStrengthIndex, PySimpleMovingAverage,
    PySlice, PySymbol, PyTradeBar, PyUniverseSettings,
};
use lean_alpha::{ActiveInsightSnapshot, IAlphaModel, Insight};
use lean_core::{DateTime, Symbol, TimeSpan};
use lean_data::Slice;
use lean_engine::FrameworkState;
use lean_portfolio_construction::{IPortfolioConstructionModel, InsightForPcm, PortfolioTarget};
use lean_sdk::data::{SharedSliceFrame, SliceView};
use lean_sdk::framework::{
    insight_from_projection, portfolio_target_from_projection, project_alpha_insight,
    project_pcm_insight, PortfolioTargetProjection,
};
use lean_sdk::securities::{read_algorithm_security_price, SymbolHandle};
use pyo3::exceptions::{PyAttributeError, PyTypeError};
use pyo3::prelude::*;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

type AlgorithmState = Arc<Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>;
type EngineFramework = Arc<Mutex<FrameworkState>>;
type InsightSnapshotCache = Arc<Mutex<ActiveInsightSnapshot>>;

static FRAMEWORKS: OnceLock<Mutex<HashMap<usize, EngineFramework>>> = OnceLock::new();
static INDICATORS: OnceLock<Mutex<HashMap<usize, Vec<RegisteredIndicator>>>> = OnceLock::new();
static INSIGHT_SNAPSHOTS: OnceLock<Mutex<HashMap<usize, InsightSnapshotCache>>> = OnceLock::new();

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PySecurityManagerCompat>()?;
    m.add_class::<PySecurityCompat>()?;
    m.add_class::<PyInsightManagerCompat>()?;
    m.add_class::<PySettingsCompat>()?;
    m.add_class::<PyCompatIndicator>()?;
    m.add_class::<PyCompatIndicatorDataPoint>()?;
    Ok(())
}

pub fn framework_for_algorithm(state: AlgorithmState) -> EngineFramework {
    let key = algorithm_key(&state);
    let snapshot = insight_snapshot_for_key(key);
    let framework = FRAMEWORKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(FrameworkState::new())))
        .clone();
    {
        let mut fw = framework.lock().unwrap();
        if !fw.has_observer() {
            fw.set_observer(Arc::new(PythonInsightObserver { snapshot }));
        }
    }
    framework
}

pub fn advance_registered_indicators(state: &AlgorithmState, slice: &Slice) {
    let key = algorithm_key(state);
    let Some(registry) = INDICATORS.get() else {
        return;
    };
    let mut indicators = registry.lock().unwrap();
    let Some(items) = indicators.get_mut(&key) else {
        return;
    };
    for bar in slice.bars.values() {
        let Some(price) = bar.close.to_f64() else {
            continue;
        };
        for item in items.iter_mut().filter(|item| item.sid == bar.symbol.id.sid) {
            item.indicator.lock().unwrap().update(price);
        }
    }
}

fn algorithm_key(state: &AlgorithmState) -> usize {
    Arc::as_ptr(state) as usize
}

fn empty_insight_snapshot() -> ActiveInsightSnapshot {
    ActiveInsightSnapshot {
        active: Arc::from([]),
        total_count: 0,
    }
}

fn insight_snapshot_for_key(key: usize) -> InsightSnapshotCache {
    INSIGHT_SNAPSHOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(empty_insight_snapshot())))
        .clone()
}

#[derive(Clone)]
struct PythonInsightObserver {
    snapshot: InsightSnapshotCache,
}

impl lean_engine::InsightObserver for PythonInsightObserver {
    fn on_insights(&self, snapshot: ActiveInsightSnapshot, _utc_now: DateTime) {
        *self.snapshot.lock().unwrap() = snapshot;
    }
}

fn py_self_any(py: Python<'_>, slf: PyRef<'_, PyQCAlgorithm>) -> Py<PyAny> {
    slf.into_pyobject(py)
        .expect("PyRef conversion is infallible")
        .into_any()
        .unbind()
}

fn first_attr<'py>(obj: &Bound<'py, PyAny>, names: &[&str]) -> PyResult<Option<Bound<'py, PyAny>>> {
    for name in names {
        match obj.getattr(name) {
            Ok(value) => return Ok(Some(value)),
            Err(err) if err.is_instance_of::<PyAttributeError>(obj.py()) => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

pub(crate) fn insight_period_from_py(period: &Bound<'_, PyAny>) -> PyResult<TimeSpan> {
    if let Ok(days) = period.extract::<i64>() {
        return Ok(TimeSpan::from_nanos(days * 86_400_000_000_000));
    }
    if let Ok(seconds) = period.call_method0("total_seconds")?.extract::<f64>() {
        if seconds.is_finite() {
            return Ok(TimeSpan::from_nanos((seconds * 1_000_000_000.0) as i64));
        }
    }
    Err(PyTypeError::new_err("period must be an int day count or datetime.timedelta"))
}

fn alpha_direction(direction: PyInsightDirection) -> lean_alpha::InsightDirection {
    match direction {
        PyInsightDirection::Up => lean_alpha::InsightDirection::Up,
        PyInsightDirection::Down => lean_alpha::InsightDirection::Down,
        PyInsightDirection::Flat => lean_alpha::InsightDirection::Flat,
    }
}

fn py_direction(direction: lean_alpha::InsightDirection) -> PyInsightDirection {
    match direction {
        lean_alpha::InsightDirection::Up => PyInsightDirection::Up,
        lean_alpha::InsightDirection::Down => PyInsightDirection::Down,
        lean_alpha::InsightDirection::Flat => PyInsightDirection::Flat,
    }
}

fn extract_insights(result: &Bound<'_, PyAny>) -> PyResult<Vec<Insight>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let mut insights = Vec::new();
    for item in result.try_iter()? {
        let item = item?;
        let projection: PyRef<'_, PyInsight> = item.extract()?;
        insights.push(insight_from_projection(projection.sdk()));
    }
    Ok(insights)
}

fn extract_targets(
    result: &Bound<'_, PyAny>,
    portfolio_value: Decimal,
    prices: &HashMap<String, Decimal>,
) -> PyResult<Vec<PortfolioTarget>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for item in result.try_iter()? {
        let item = item?;
        let projection: PyRef<'_, PyPortfolioTarget> = item.extract()?;
        if let Some(target) =
            portfolio_target_from_projection(projection.sdk(), portfolio_value, prices)
        {
            targets.push(target);
        }
    }
    Ok(targets)
}

struct PythonAlphaModelAdapter {
    model: Py<PyAny>,
    algorithm: Py<PyAny>,
    slice_frame: SharedSliceFrame,
    py_slice: Py<PyAny>,
    name: String,
}

impl PythonAlphaModelAdapter {
    fn new(py: Python<'_>, model: Py<PyAny>, algorithm: Py<PyAny>) -> PyResult<Self> {
        let slice_frame = SharedSliceFrame::new();
        let py_slice = Py::new(py, PySlice::from_view(SliceView::new(slice_frame.clone())))?
            .into_any();
        let name = model
            .bind(py)
            .getattr("Name")
            .ok()
            .and_then(|value| value.extract::<String>().ok())
            .unwrap_or_else(|| "PythonAlphaModel".to_string());
        Ok(Self {
            model,
            algorithm,
            slice_frame,
            py_slice,
            name,
        })
    }
}

impl IAlphaModel for PythonAlphaModelAdapter {
    fn update(&mut self, slice: &Slice, _securities: &[Symbol]) -> Vec<Insight> {
        self.slice_frame.set_current(Arc::new(slice.clone()));
        Python::attach(|py| -> PyResult<Vec<Insight>> {
            let model = self.model.bind(py);
            let Some(callback) = first_attr(model, &["Update", "update"])? else {
                return Ok(Vec::new());
            };
            let result = callback.call1((self.algorithm.clone_ref(py), self.py_slice.clone_ref(py)))?;
            extract_insights(&result)
        })
        .unwrap_or_else(|error| {
            tracing::error!("Python alpha model {} failed: {error}", self.name);
            Vec::new()
        })
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}

    fn name(&self) -> &str {
        &self.name
    }
}

struct PythonPortfolioConstructionModelAdapter {
    model: Py<PyAny>,
    algorithm: Py<PyAny>,
    name: String,
}

impl PythonPortfolioConstructionModelAdapter {
    fn new(py: Python<'_>, model: Py<PyAny>, algorithm: Py<PyAny>) -> Self {
        let name = model
            .bind(py)
            .getattr("Name")
            .ok()
            .and_then(|value| value.extract::<String>().ok())
            .unwrap_or_else(|| "PythonPortfolioConstructionModel".to_string());
        Self {
            model,
            algorithm,
            name,
        }
    }
}

impl IPortfolioConstructionModel for PythonPortfolioConstructionModelAdapter {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        Python::attach(|py| -> PyResult<Vec<PortfolioTarget>> {
            let model = self.model.bind(py);
            let Some(callback) = first_attr(model, &["CreateTargets", "create_targets"])? else {
                return Ok(Vec::new());
            };
            let py_insights = insights
                .iter()
                .map(|insight| {
                    Py::new(
                        py,
                        PyInsight::from_view(project_pcm_insight(insight, DateTime::now())),
                    )
                    .map(|value| value.into_any())
                })
                .collect::<PyResult<Vec<_>>>()?;
            let result = callback.call1((self.algorithm.clone_ref(py), py_insights))?;
            extract_targets(&result, portfolio_value, prices)
        })
        .unwrap_or_else(|error| {
            tracing::error!("Python PCM {} failed: {error}", self.name);
            Vec::new()
        })
    }

    fn use_all_active_insights(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        &self.name
    }
}

pub fn qc_add_alpha(slf: PyRef<'_, PyQCAlgorithm>, py: Python<'_>, model: Py<PyAny>) -> PyResult<()> {
    let algorithm = py_self_any(py, slf);
    let state = adapter_state_from_algorithm(&algorithm, py)?;
    let adapter = PythonAlphaModelAdapter::new(py, model, algorithm)?;
    framework_for_algorithm(state)
        .lock()
        .unwrap()
        .alpha_models
        .push(Box::new(adapter));
    Ok(())
}

pub fn qc_set_portfolio_construction(
    slf: PyRef<'_, PyQCAlgorithm>,
    py: Python<'_>,
    model: Py<PyAny>,
) -> PyResult<()> {
    let algorithm = py_self_any(py, slf);
    let state = adapter_state_from_algorithm(&algorithm, py)?;
    let adapter = PythonPortfolioConstructionModelAdapter::new(py, model, algorithm.clone_ref(py));
    framework_for_algorithm(state).lock().unwrap().pcm = Box::new(adapter);
    Ok(())
}

pub fn qc_set_execution(algorithm: &PyQCAlgorithm, _model: Py<PyAny>) {
    framework_for_algorithm(algorithm.inner.inner())
        .lock()
        .unwrap()
        .exec_model = Box::new(lean_execution::ImmediateExecutionModel::new());
}

pub fn qc_set_risk_management(algorithm: &PyQCAlgorithm, _model: Py<PyAny>) {
    framework_for_algorithm(algorithm.inner.inner())
        .lock()
        .unwrap()
        .risk_model = Box::new(lean_risk::NullRiskManagement);
}

pub fn qc_get_parameter(_algorithm: &PyQCAlgorithm, _key: String, default: Option<String>) -> Option<String> {
    default
}

pub fn qc_set_warm_up(
    algorithm: &PyQCAlgorithm,
    n: i64,
    resolution: Option<lean_core::Resolution>,
) {
    algorithm.inner.set_warm_up_int(n, resolution);
}

pub fn qc_set_start_date(algorithm: &PyQCAlgorithm, year: i32, month: u32, day: u32) {
    algorithm.inner.set_start_date(year, month, day);
}

pub fn qc_set_end_date(algorithm: &PyQCAlgorithm, year: i32, month: u32, day: u32) {
    algorithm.inner.set_end_date(year, month, day);
}

pub fn qc_set_cash(algorithm: &PyQCAlgorithm, amount: f64) {
    algorithm.inner.set_cash(amount);
}

pub fn qc_set_benchmark(algorithm: &PyQCAlgorithm, ticker: String) {
    algorithm.inner.set_benchmark(ticker);
}

pub fn qc_log(algorithm: &PyQCAlgorithm, message: String) {
    algorithm.inner.log(message);
}

pub fn qc_debug(algorithm: &PyQCAlgorithm, message: String) {
    algorithm.inner.debug(message);
}

pub fn qc_error(algorithm: &PyQCAlgorithm, message: String) {
    algorithm.inner.error(message);
}

pub fn qc_add_data(
    algorithm: &PyQCAlgorithm,
    source_type: String,
    ticker: String,
    resolution: lean_core::Resolution,
    properties: Option<HashMap<String, String>>,
) -> PySymbol {
    PySymbol::from_view(algorithm.inner.add_data_with_properties(
        source_type,
        ticker,
        resolution,
        properties.unwrap_or_default(),
    ))
}

pub fn qc_sma(algorithm: &PyQCAlgorithm, symbol: PySymbol, period: usize) -> PyCompatIndicator {
    algorithm.compat_indicator(symbol, period, CompatIndicatorKind::Sma)
}

pub fn qc_ema(algorithm: &PyQCAlgorithm, symbol: PySymbol, period: usize) -> PyCompatIndicator {
    algorithm.compat_indicator(symbol, period, CompatIndicatorKind::Ema)
}

pub fn qc_std(algorithm: &PyQCAlgorithm, symbol: PySymbol, period: usize) -> PyCompatIndicator {
    algorithm.compat_indicator(symbol, period, CompatIndicatorKind::Std)
}

pub fn qc_momp(algorithm: &PyQCAlgorithm, symbol: PySymbol, period: usize) -> PyCompatIndicator {
    algorithm.compat_indicator(symbol, period, CompatIndicatorKind::Momp)
}

pub fn qc_rsi(algorithm: &PyQCAlgorithm, symbol: PySymbol, period: usize) -> PyCompatIndicator {
    algorithm.compat_indicator(symbol, period, CompatIndicatorKind::Rsi)
}

pub fn qc_securities(algorithm: &PyQCAlgorithm) -> PySecurityManagerCompat {
    PySecurityManagerCompat {
        state: algorithm.inner.inner(),
    }
}

pub fn qc_insights(algorithm: &PyQCAlgorithm) -> PyInsightManagerCompat {
    PyInsightManagerCompat {
        snapshot: insight_snapshot_for_key(algorithm_key(&algorithm.inner.inner())),
    }
}

pub fn qc_settings(_algorithm: &PyQCAlgorithm) -> PySettingsCompat {
    PySettingsCompat
}

pub fn qc_utc_time(algorithm: &PyQCAlgorithm) -> chrono::NaiveDateTime {
    algorithm.inner.utc_time()
}

pub fn qc_time(algorithm: &PyQCAlgorithm) -> chrono::NaiveDateTime {
    algorithm.inner.current_time()
}

impl PyQCAlgorithm {
    fn compat_indicator(
        &self,
        symbol: PySymbol,
        period: usize,
        kind: CompatIndicatorKind,
    ) -> PyCompatIndicator {
        let indicator = Arc::new(Mutex::new(CompatIndicatorState::new(kind, period)));
        let key = algorithm_key(&self.inner.inner());
        INDICATORS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .entry(key)
            .or_default()
            .push(RegisteredIndicator {
                sid: symbol.sdk().inner().id.sid,
                indicator: indicator.clone(),
            });
        PyCompatIndicator { indicator }
    }
}

fn adapter_state_from_algorithm(algorithm: &Py<PyAny>, py: Python<'_>) -> PyResult<AlgorithmState> {
    let bound = algorithm.bind(py);
    let alg: PyRef<'_, PyQCAlgorithm> = bound.extract()?;
    Ok(alg.inner.inner())
}

#[pyclass(name = "SecurityManagerView")]
#[derive(Clone)]
pub struct PySecurityManagerCompat {
    state: AlgorithmState,
}

#[pymethods]
impl PySecurityManagerCompat {
    fn __getitem__(&self, symbol: PySymbol) -> PySecurityCompat {
        PySecurityCompat {
            state: self.state.clone(),
            symbol: symbol.sdk().inner().clone(),
        }
    }
}

#[pyclass(name = "SecurityView")]
#[derive(Clone)]
pub struct PySecurityCompat {
    state: AlgorithmState,
    symbol: Symbol,
}

#[pymethods]
impl PySecurityCompat {
    #[getter(Price)]
    fn price_pascal(&self) -> f64 {
        read_algorithm_security_price(&self.state, &self.symbol).unwrap_or(0.0)
    }

    #[getter(price)]
    fn price(&self) -> f64 {
        self.price_pascal()
    }

    #[getter(Symbol)]
    fn symbol_pascal(&self) -> PySymbol {
        PySymbol::from_view(SymbolHandle::new(self.symbol.clone()))
    }
}

#[pyclass(name = "InsightManagerView")]
#[derive(Clone)]
pub struct PyInsightManagerCompat {
    snapshot: InsightSnapshotCache,
}

#[pymethods]
impl PyInsightManagerCompat {
    fn get_insights(&self) -> Vec<PyInsight> {
        self.get_insights_pascal()
    }

    fn get_active_insights(&self, utc_time: Option<chrono::NaiveDateTime>) -> Vec<PyInsight> {
        self.get_active_insights_pascal(utc_time)
    }

    #[pyo3(name = "GetInsights")]
    fn get_insights_pascal(&self) -> Vec<PyInsight> {
        let active = self.snapshot.lock().unwrap().active.clone();
        active
            .iter()
            .map(|insight| PyInsight::from_view(project_alpha_insight(insight)))
            .collect()
    }

    #[pyo3(name = "GetActiveInsights")]
    fn get_active_insights_pascal(&self, utc_time: Option<chrono::NaiveDateTime>) -> Vec<PyInsight> {
        let utc = utc_time
            .map(lean_core::NanosecondTimestamp::from)
            .unwrap_or_else(DateTime::now);
        self.snapshot
            .lock()
            .unwrap()
            .clone()
            .active
            .iter()
            .filter(|insight| insight.is_active(utc))
            .map(|insight| PyInsight::from_view(project_alpha_insight(insight)))
            .collect()
    }
}

#[pyclass(name = "AlgorithmSettingsView")]
#[derive(Clone, Copy)]
pub struct PySettingsCompat;

#[pymethods]
impl PySettingsCompat {
    #[setter(minimum_order_margin_portfolio_percentage)]
    fn set_minimum_order_margin_portfolio_percentage_snake(&self, _value: f64) {}

    #[setter(MinimumOrderMarginPortfolioPercentage)]
    fn set_minimum_order_margin_portfolio_percentage(&self, _value: f64) {}
}

pub fn symbol_value_pascal(symbol: &PySymbol) -> &str {
    symbol.sdk().value()
}

pub fn symbol_id_pascal(symbol: &PySymbol) -> u64 {
    symbol.sdk().sid()
}

pub fn insight_price_pascal(
    symbol: PySymbol,
    period: &Bound<'_, PyAny>,
    direction: PyInsightDirection,
    magnitude: Option<f64>,
    confidence: Option<f64>,
    source_model: Option<String>,
) -> PyResult<PyInsight> {
    let period = insight_period_from_py(period)?;
    let insight = Insight::new(
        symbol.sdk().inner().clone(),
        alpha_direction(direction),
        period,
        magnitude.and_then(Decimal::from_f64),
        confidence.and_then(Decimal::from_f64),
        source_model.as_deref().unwrap_or(""),
    );
    Ok(PyInsight::from_view(project_alpha_insight(&insight)))
}

pub fn insight_symbol(insight: &PyInsight) -> PySymbol {
    PySymbol::from_view(SymbolHandle::new(insight.sdk().symbol.clone()))
}

pub fn insight_direction(insight: &PyInsight) -> PyInsightDirection {
    py_direction(insight.sdk().direction)
}

pub fn insight_magnitude(insight: &PyInsight) -> Option<f64> {
    insight.sdk().magnitude
}

pub fn insight_confidence(insight: &PyInsight) -> Option<f64> {
    insight.sdk().confidence
}

pub fn insight_source_model(insight: &PyInsight) -> String {
    insight.sdk().source_model.clone()
}

pub fn insight_score_direction(insight: &PyInsight) -> Option<f64> {
    insight.sdk().score_direction
}

pub fn portfolio_target_percent(
    _algorithm: &Bound<'_, PyAny>,
    symbol: PySymbol,
    percent: f64,
) -> PyPortfolioTarget {
    PyPortfolioTarget::from_view(PortfolioTargetProjection {
        symbol: symbol.sdk().inner().clone(),
        quantity: None,
        percent: Some(percent),
        tag: String::new(),
    })
}

pub fn tradebar_end_time(bar: &PyTradeBar) -> chrono::NaiveDateTime {
    bar.sdk().end_time()
}

pub fn tradebar_time(bar: &PyTradeBar) -> chrono::NaiveDateTime {
    bar.sdk().time()
}

pub fn tradebar_close(bar: &PyTradeBar) -> f64 {
    bar.sdk().close()
}

pub fn tradebar_open(bar: &PyTradeBar) -> f64 {
    bar.sdk().open()
}

pub fn tradebar_high(bar: &PyTradeBar) -> f64 {
    bar.sdk().high()
}

pub fn tradebar_low(bar: &PyTradeBar) -> f64 {
    bar.sdk().low()
}

pub fn quotebar_close(bar: &PyQuoteBar) -> f64 {
    bar.sdk().close()
}

pub fn bar_close(bar: &PyBar) -> f64 {
    bar.sdk().close
}

pub fn indicator_data_point_value(point: &PyIndicatorDataPoint) -> f64 {
    point.sdk().value()
}

pub fn sma_is_ready(indicator: &PySimpleMovingAverage) -> bool {
    indicator.sdk().is_ready()
}

pub fn sma_current(indicator: &PySimpleMovingAverage) -> PyIndicatorDataPoint {
    PyIndicatorDataPoint::from_view(indicator.sdk().current())
}

pub fn ema_is_ready(indicator: &PyExponentialMovingAverage) -> bool {
    indicator.sdk().is_ready()
}

pub fn ema_current(indicator: &PyExponentialMovingAverage) -> PyIndicatorDataPoint {
    PyIndicatorDataPoint::from_view(indicator.sdk().current())
}

pub fn rsi_is_ready(indicator: &PyRelativeStrengthIndex) -> bool {
    indicator.sdk().is_ready()
}

pub fn rsi_current(indicator: &PyRelativeStrengthIndex) -> PyIndicatorDataPoint {
    PyIndicatorDataPoint::from_view(indicator.sdk().current())
}

pub fn universe_set_resolution(settings: &PyUniverseSettings, resolution: lean_core::Resolution) {
    settings.sdk().set_resolution(resolution);
}

pub fn universe_set_leverage(settings: &PyUniverseSettings, leverage: f64) {
    settings.sdk().set_leverage(leverage);
}

#[derive(Clone, Copy)]
enum CompatIndicatorKind {
    Sma,
    Ema,
    Std,
    Momp,
    Rsi,
}

#[derive(Clone)]
struct RegisteredIndicator {
    sid: u64,
    indicator: Arc<Mutex<CompatIndicatorState>>,
}

struct CompatIndicatorState {
    kind: CompatIndicatorKind,
    period: usize,
    samples: usize,
    values: VecDeque<f64>,
    current: f64,
    ema: Option<f64>,
}

impl CompatIndicatorState {
    fn new(kind: CompatIndicatorKind, period: usize) -> Self {
        Self {
            kind,
            period: period.max(1),
            samples: 0,
            values: VecDeque::new(),
            current: 0.0,
            ema: None,
        }
    }

    fn update(&mut self, value: f64) {
        if !value.is_finite() {
            return;
        }
        self.samples += 1;
        self.values.push_back(value);
        let keep = self.period + 1;
        while self.values.len() > keep {
            self.values.pop_front();
        }
        self.current = match self.kind {
            CompatIndicatorKind::Sma => {
                let n = self.values.len().min(self.period);
                let sum: f64 = self.values.iter().rev().take(n).sum();
                sum / n as f64
            }
            CompatIndicatorKind::Ema => {
                let k = 2.0 / (self.period as f64 + 1.0);
                let ema = self.ema.map(|prev| value * k + prev * (1.0 - k)).unwrap_or(value);
                self.ema = Some(ema);
                ema
            }
            CompatIndicatorKind::Std => {
                let n = self.values.len().min(self.period);
                let recent: Vec<f64> = self.values.iter().rev().take(n).copied().collect();
                let mean = recent.iter().sum::<f64>() / n as f64;
                let var = recent
                    .iter()
                    .map(|x| {
                        let d = *x - mean;
                        d * d
                    })
                    .sum::<f64>()
                    / n as f64;
                var.sqrt()
            }
            CompatIndicatorKind::Momp => {
                if self.values.len() > self.period {
                    let old = self.values[0];
                    if old.abs() > f64::EPSILON {
                        (value - old) / old
                    } else {
                        0.0
                    }
                } else {
                    0.0
                }
            }
            CompatIndicatorKind::Rsi => 50.0,
        };
    }

    fn is_ready(&self) -> bool {
        match self.kind {
            CompatIndicatorKind::Momp | CompatIndicatorKind::Rsi => self.samples > self.period,
            CompatIndicatorKind::Sma | CompatIndicatorKind::Ema | CompatIndicatorKind::Std => {
                self.samples >= self.period
            }
        }
    }
}

#[pyclass(name = "CompatIndicator")]
#[derive(Clone)]
pub struct PyCompatIndicator {
    indicator: Arc<Mutex<CompatIndicatorState>>,
}

#[pymethods]
impl PyCompatIndicator {
    #[getter(IsReady)]
    fn is_ready_pascal(&self) -> bool {
        self.indicator.lock().unwrap().is_ready()
    }

    #[getter(is_ready)]
    fn is_ready(&self) -> bool {
        self.is_ready_pascal()
    }

    #[getter(Current)]
    fn current_pascal(&self) -> PyCompatIndicatorDataPoint {
        PyCompatIndicatorDataPoint {
            value: self.indicator.lock().unwrap().current,
        }
    }

    #[getter(current)]
    fn current(&self) -> PyCompatIndicatorDataPoint {
        self.current_pascal()
    }
}

#[pyclass(name = "CompatIndicatorDataPoint")]
#[derive(Clone, Copy)]
pub struct PyCompatIndicatorDataPoint {
    value: f64,
}

#[pymethods]
impl PyCompatIndicatorDataPoint {
    #[getter(Value)]
    fn value_pascal(&self) -> f64 {
        self.value
    }

    #[getter(value)]
    fn value(&self) -> f64 {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::{CompatIndicatorKind, CompatIndicatorState};

    #[test]
    fn momentum_and_rsi_require_period_plus_one_samples() {
        for kind in [CompatIndicatorKind::Momp, CompatIndicatorKind::Rsi] {
            let mut indicator = CompatIndicatorState::new(kind, 3);
            for value in [1.0, 2.0, 3.0] {
                indicator.update(value);
            }
            assert!(!indicator.is_ready());
            indicator.update(4.0);
            assert!(indicator.is_ready());
        }
    }

    #[test]
    fn moving_window_indicators_are_ready_at_period_samples() {
        for kind in [
            CompatIndicatorKind::Sma,
            CompatIndicatorKind::Ema,
            CompatIndicatorKind::Std,
        ] {
            let mut indicator = CompatIndicatorState::new(kind, 3);
            for value in [1.0, 2.0] {
                indicator.update(value);
            }
            assert!(!indicator.is_ready());
            indicator.update(3.0);
            assert!(indicator.is_ready());
        }
    }
}
