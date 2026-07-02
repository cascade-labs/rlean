//! Python framework model adapters and registration helpers.

use lean_algorithm::lifecycle::CustomUniverseSelectFn;
use lean_algorithm::FrameworkModelRegistry;
use lean_alpha::{IAlphaModel, Insight};
use lean_core::{DateTime, Market, Resolution, SecurityType, Symbol};
use lean_data::{CustomDataPoint, CustomDataQuery, Slice};
use lean_execution::{ExecutionContext, ExecutionTarget, IExecutionModel, OrderRequest};
use lean_portfolio_construction::{IPortfolioConstructionModel, InsightForPcm, PortfolioTarget};
use lean_risk::risk_management::{PortfolioTarget as RiskPortfolioTarget, RiskManagementModel};
use pyo3::exceptions::{PyAttributeError, PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyAnyMethods, PyList};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::algorithm::{AlgorithmApi, AlgorithmHandle};
use crate::data::{CustomDataPointView, SharedSliceFrame, SliceView};
use crate::framework::{
    create_immediate_execution_model, create_null_risk_management_model, insight_from_projection,
    portfolio_target_from_projection, portfolio_target_projection_from_execution_target,
    portfolio_target_projection_from_risk_target, project_alpha_insight, project_pcm_insight,
    ImmediateExecutionModel, InsightProjection, NullRiskManagementModel, PortfolioTargetProjection,
};
use crate::securities::SymbolHandle;
use crate::universe::ScheduledUniverseDescriptor;

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

fn extract_insights(result: &Bound<'_, PyAny>) -> PyResult<Vec<Insight>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let mut insights = Vec::new();
    for item in result.try_iter()? {
        let item = item?;
        let projection: PyRef<'_, InsightProjection> = item.extract()?;
        insights.push(insight_from_projection(&projection));
    }
    Ok(insights)
}

fn extract_targets(
    result: &Bound<'_, PyAny>,
    portfolio_value: Decimal,
    prices: &HashMap<u64, Decimal>,
) -> PyResult<Vec<PortfolioTarget>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for item in result.try_iter()? {
        let item = item?;
        let projection = item.extract::<PyRef<'_, PortfolioTargetProjection>>()?;
        if let Some(target) = portfolio_target_from_projection(&projection, portfolio_value, prices)
        {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn extract_risk_targets(result: &Bound<'_, PyAny>) -> PyResult<Vec<RiskPortfolioTarget>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let mut targets = Vec::new();
    for item in result.try_iter()? {
        let item = item?;
        let projection = item.extract::<PyRef<'_, PortfolioTargetProjection>>()?;
        if let Some(quantity) = projection.quantity.and_then(Decimal::from_f64) {
            targets.push(RiskPortfolioTarget::new(
                projection.symbol.clone(),
                quantity,
            ));
        }
    }
    Ok(targets)
}

fn py_target_list<'py>(
    py: Python<'py>,
    targets: &[PortfolioTargetProjection],
) -> PyResult<Bound<'py, PyList>> {
    let py_targets = PyList::empty(py);
    for target in targets {
        py_targets.append(Py::new(py, target.clone())?)?;
    }
    Ok(py_targets)
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
        let py_slice = Py::new(py, SliceView::new(slice_frame.clone()))?.into_any();
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
            let result =
                callback.call1((self.algorithm.clone_ref(py), self.py_slice.clone_ref(py)))?;
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
        prices: &HashMap<u64, Decimal>,
    ) -> Vec<PortfolioTarget> {
        Python::attach(|py| -> PyResult<Vec<PortfolioTarget>> {
            let model = self.model.bind(py);
            let Some(callback) = first_attr(model, &["CreateTargets", "create_targets"])? else {
                return Ok(Vec::new());
            };
            let py_insights = insights
                .iter()
                .map(|insight| {
                    Py::new(py, project_pcm_insight(insight, DateTime::now()))
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

struct PythonExecutionModelAdapter {
    model: Py<PyAny>,
    algorithm: Py<PyAny>,
    name: String,
}

impl PythonExecutionModelAdapter {
    fn new(py: Python<'_>, model: Py<PyAny>, algorithm: Py<PyAny>) -> Self {
        let name = model
            .bind(py)
            .getattr("Name")
            .ok()
            .and_then(|value| value.extract::<String>().ok())
            .unwrap_or_else(|| "PythonExecutionModel".to_string());
        Self {
            model,
            algorithm,
            name,
        }
    }
}

impl IExecutionModel for PythonExecutionModelAdapter {
    fn execute(
        &mut self,
        targets: &[ExecutionTarget],
        _context: &ExecutionContext<'_>,
    ) -> Vec<OrderRequest> {
        Python::attach(|py| -> PyResult<Vec<OrderRequest>> {
            let model = self.model.bind(py);
            let Some(callback) = first_attr(model, &["Execute", "execute"])? else {
                return Ok(Vec::new());
            };
            let py_targets = py_target_list(
                py,
                &targets
                    .iter()
                    .map(portfolio_target_projection_from_execution_target)
                    .collect::<Vec<_>>(),
            )?;
            callback.call1((self.algorithm.clone_ref(py), py_targets))?;
            Ok(Vec::new())
        })
        .unwrap_or_else(|error| {
            tracing::error!("Python execution model {} failed: {error}", self.name);
            Vec::new()
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

struct PythonRiskManagementModelAdapter {
    model: Py<PyAny>,
    algorithm: Py<PyAny>,
}

impl PythonRiskManagementModelAdapter {
    fn new(_py: Python<'_>, model: Py<PyAny>, algorithm: Py<PyAny>) -> Self {
        Self { model, algorithm }
    }
}

impl RiskManagementModel for PythonRiskManagementModelAdapter {
    fn manage_risk(&mut self, targets: &[RiskPortfolioTarget]) -> Vec<RiskPortfolioTarget> {
        Python::attach(|py| -> PyResult<Vec<RiskPortfolioTarget>> {
            let model = self.model.bind(py);
            let Some(callback) = first_attr(model, &["ManageRisk", "manage_risk"])? else {
                return Ok(targets.to_vec());
            };
            let py_targets = py_target_list(
                py,
                &targets
                    .iter()
                    .map(portfolio_target_projection_from_risk_target)
                    .collect::<Vec<_>>(),
            )?;
            let result = callback.call1((self.algorithm.clone_ref(py), py_targets))?;
            let adjusted = extract_risk_targets(&result)?;
            if adjusted.is_empty() && !result.is_none() {
                Ok(Vec::new())
            } else if adjusted.is_empty() {
                Ok(targets.to_vec())
            } else {
                Ok(adjusted)
            }
        })
        .unwrap_or_else(|error| {
            tracing::error!("Python risk model failed: {error}");
            targets.to_vec()
        })
    }
}

pub fn framework_registry(handle: &AlgorithmHandle) -> PyResult<Arc<dyn FrameworkModelRegistry>> {
    handle
        .runtime_services()
        .framework_registry()
        .ok_or_else(|| PyRuntimeError::new_err("framework registry unavailable"))
}

fn algorithm_py_object<'py>(
    py: Python<'py>,
    handle: &AlgorithmHandle,
) -> PyResult<Bound<'py, PyAny>> {
    let state_key = custom_universe_state_key(&handle.inner());
    if let Some(strategy) = python_strategy_object(state_key, py) {
        return Ok(strategy);
    }
    Bound::new(py, handle.clone()).map(|bound| bound.into_any())
}

pub fn register_alpha(py: Python<'_>, handle: &AlgorithmHandle, model: Py<PyAny>) -> PyResult<()> {
    let algorithm = algorithm_py_object(py, handle)?.unbind();
    let adapter = PythonAlphaModelAdapter::new(py, model, algorithm)?;
    framework_registry(handle)?.add_alpha_model(Box::new(adapter));
    Ok(())
}

pub fn register_portfolio_construction(
    py: Python<'_>,
    handle: &AlgorithmHandle,
    model: Py<PyAny>,
) -> PyResult<()> {
    let algorithm = algorithm_py_object(py, handle)?.unbind();
    let adapter = PythonPortfolioConstructionModelAdapter::new(py, model, algorithm);
    framework_registry(handle)?.set_portfolio_construction_model(Box::new(adapter));
    Ok(())
}

pub fn register_execution(
    py: Python<'_>,
    handle: &AlgorithmHandle,
    model: Py<PyAny>,
) -> PyResult<()> {
    if model.bind(py).is_instance_of::<ImmediateExecutionModel>() {
        framework_registry(handle)?.set_execution_model(create_immediate_execution_model());
        return Ok(());
    }
    let algorithm = algorithm_py_object(py, handle)?.unbind();
    let adapter = PythonExecutionModelAdapter::new(py, model, algorithm);
    framework_registry(handle)?.set_execution_model(Box::new(adapter));
    Ok(())
}

pub fn register_risk_management(
    py: Python<'_>,
    handle: &AlgorithmHandle,
    model: Py<PyAny>,
) -> PyResult<()> {
    if model.bind(py).is_instance_of::<NullRiskManagementModel>() {
        framework_registry(handle)?.set_risk_management_model(create_null_risk_management_model());
        return Ok(());
    }
    let algorithm = algorithm_py_object(py, handle)?.unbind();
    let adapter = PythonRiskManagementModelAdapter::new(py, model, algorithm);
    framework_registry(handle)?.set_risk_management_model(Box::new(adapter));
    Ok(())
}

pub fn custom_universe_state_key(
    state: &Arc<Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>,
) -> usize {
    Arc::as_ptr(state) as usize
}

fn python_strategy_registry() -> &'static Mutex<HashMap<usize, Py<PyAny>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<usize, Py<PyAny>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn register_python_strategy_object(state_key: usize, strategy: Py<PyAny>) {
    python_strategy_registry()
        .lock()
        .unwrap()
        .insert(state_key, strategy);
}

fn python_strategy_object<'py>(state_key: usize, py: Python<'py>) -> Option<Bound<'py, PyAny>> {
    python_strategy_registry()
        .lock()
        .unwrap()
        .get(&state_key)
        .map(|strategy| strategy.clone_ref(py).bind(py).clone())
}

pub fn has_custom_universes(handle: &AlgorithmHandle) -> bool {
    handle.runtime_services().has_custom_universe_selectors()
}

pub fn custom_universe_resolution_for(handle: &AlgorithmHandle) -> Option<Resolution> {
    handle
        .runtime_services()
        .custom_universe_selector_resolution()
}

fn extract_selector_tickers(
    result: &Bound<'_, PyAny>,
    descriptor: &ScheduledUniverseDescriptor,
) -> PyResult<Vec<Symbol>> {
    if result.is_none() {
        return Ok(Vec::new());
    }
    let mut symbols = Vec::new();
    for item in result.try_iter()? {
        let item = item?;
        if let Ok(ticker) = item.extract::<String>() {
            let ticker = ticker.trim();
            if !ticker.is_empty() {
                symbols.push(descriptor.create_symbol(ticker));
            }
            continue;
        }
        if let Ok(symbol) = item.extract::<SymbolHandle>() {
            symbols.push(symbol.into_inner());
            continue;
        }
        if let Ok(value) = item.getattr("value") {
            if let Ok(ticker) = value.extract::<String>() {
                let ticker = ticker.trim();
                if !ticker.is_empty() {
                    symbols.push(descriptor.create_symbol(ticker));
                }
            }
        }
    }
    Ok(symbols)
}

pub fn register_custom_universe(
    _py: Python<'_>,
    handle: &AlgorithmHandle,
    source_type: String,
    ticker: String,
    resolution: Resolution,
    selector: Py<PyAny>,
) -> PyResult<()> {
    let settings = handle.universe_settings().snapshot();
    let descriptor = ScheduledUniverseDescriptor::custom_data(
        source_type.clone(),
        ticker.clone(),
        resolution,
        settings.clone(),
        SecurityType::Equity,
        Market::usa(),
    );
    {
        let state = handle.inner();
        let mut algorithm = state.lock().unwrap();
        AlgorithmApi::new(&mut algorithm).add_custom_universe_data(
            &source_type,
            &ticker,
            resolution,
            descriptor.custom_properties.clone(),
        );
    }

    let callback = selector;
    let descriptor_for_select = descriptor.clone();
    let select: CustomUniverseSelectFn = Arc::new(move |points| {
        Python::attach(|py| -> PyResult<Vec<Symbol>> {
            let py_points = PyList::empty(py);
            for point in points {
                py_points.append(CustomDataPointView::new(point.clone()))?;
            }
            let result = callback.bind(py).call1((py_points,))?;
            extract_selector_tickers(&result, &descriptor_for_select)
        })
        .unwrap_or_default()
    });
    handle.runtime_services().register_custom_universe_selector(
        lean_algorithm::lifecycle::CustomUniverseSelectorRegistrationRequest {
            source_type,
            ticker,
            resolution,
            settings_resolution: settings.resolution,
            minimum_time_in_universe_secs: settings.minimum_time_in_universe_secs,
            select,
        },
    );
    Ok(())
}

pub fn set_custom_data_symbols(
    handle: &AlgorithmHandle,
    source_type: &str,
    ticker: &str,
    symbols: Vec<String>,
) -> bool {
    let query = CustomDataQuery {
        symbols: Some(symbols),
        ..CustomDataQuery::default()
    };
    let state = handle.inner();
    let algorithm = state.lock().unwrap();
    algorithm
        .subscription_manager
        .set_custom_dynamic_query(source_type, ticker, query)
}

pub fn run_custom_universe_selectors_for_handle(
    handle: &AlgorithmHandle,
    utc_ns: i64,
    resolution: Resolution,
    custom_data: &HashMap<String, Vec<CustomDataPoint>>,
) -> PyResult<Vec<lean_algorithm::lifecycle::UniverseSelection>> {
    Ok(handle
        .runtime_services()
        .run_custom_universe_selections(utc_ns, resolution, custom_data))
}

pub fn insight_period_from_py(period: &Bound<'_, PyAny>) -> PyResult<lean_core::TimeSpan> {
    if let Ok(days) = period.extract::<i64>() {
        return Ok(lean_core::TimeSpan::from_nanos(days * 86_400_000_000_000));
    }
    if let Ok(seconds) = period.call_method0("total_seconds")?.extract::<f64>() {
        if seconds.is_finite() {
            return Ok(lean_core::TimeSpan::from_nanos(
                (seconds * 1_000_000_000.0) as i64,
            ));
        }
    }
    Err(PyTypeError::new_err(
        "period must be an int day count or datetime.timedelta",
    ))
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "InsightCollection"))]
#[derive(Clone)]
pub struct InsightCollectionHandle {
    snapshot: Arc<std::sync::Mutex<lean_alpha::ActiveInsightSnapshot>>,
}

impl InsightCollectionHandle {
    pub fn from_registry(registry: &Arc<dyn FrameworkModelRegistry>) -> Self {
        Self {
            snapshot: registry.insight_snapshot(),
        }
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl InsightCollectionHandle {
    #[pyo3(name = "get_insights")]
    fn py_get_insights(&self) -> Vec<InsightProjection> {
        self.snapshot
            .lock()
            .unwrap()
            .active
            .iter()
            .map(project_alpha_insight)
            .collect()
    }

    #[pyo3(name = "get_active_insights", signature = (utc_time=None))]
    fn py_get_active_insights(
        &self,
        utc_time: Option<chrono::NaiveDateTime>,
    ) -> Vec<InsightProjection> {
        let utc = utc_time.map(DateTime::from).unwrap_or_else(DateTime::now);
        self.snapshot
            .lock()
            .unwrap()
            .active
            .iter()
            .filter(|insight| insight.is_active(utc))
            .map(project_alpha_insight)
            .collect()
    }
}
