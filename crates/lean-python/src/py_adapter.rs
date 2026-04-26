use crate::charting::ChartCollection;
use crate::py_data::{PySlice, PyTradeBar, SliceProxy};
use crate::py_framework::FrameworkState;
use crate::py_orders::PyOrderEvent;
use crate::py_qc_algorithm::{IndicatorRegistry, PyQcAlgorithm};
use crate::py_universe::{PyScheduledUniverse, PySecurityChanges};
use lean_algorithm::algorithm::{AlgorithmStatus, IAlgorithm, SecurityChanges};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{DateTime, Result as LeanResult, Symbol};
use lean_data::Slice;
use lean_orders::{Order, OrderEvent};
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};

/// Bridges a Python strategy object to the Rust `IAlgorithm` trait.
/// Holds both the Python object (for calling `OnData` etc.) and
/// the shared QcAlgorithm state (for reading dates, portfolio, etc.).
pub struct PyAlgorithmAdapter {
    /// The Python strategy instance (subclass of QcAlgorithm).
    py_obj: Py<PyAny>,
    /// Shared inner state — same Arc as inside the Python QcAlgorithm instance.
    pub inner: Arc<Mutex<QcAlgorithm>>,
    /// Shared chart collection — populated by the Python strategy via self.plot().
    pub charts: Arc<Mutex<ChartCollection>>,
    /// Algorithm Framework models — shared with PyQcAlgorithm so that
    /// models registered in Initialize() are visible to the runner.
    pub framework: Arc<Mutex<FrameworkState>>,
    /// Indicator registry — auto-updated before every OnData call.
    pub indicators: Arc<Mutex<IndicatorRegistry>>,
    /// Scheduled/user-defined universes registered from Python.
    pub universes: Arc<Mutex<Vec<Py<PyScheduledUniverse>>>>,
    /// Cached after Initialize().
    pub name: String,
}

impl PyAlgorithmAdapter {
    /// Hot-path `OnData` using a pre-allocated `SliceProxy`.
    ///
    /// Updates the proxy's bar objects in-place (zero allocation), then calls
    /// Python's `OnData` with the same stable Python object as every other day.
    /// Must be called with the GIL already held via `Python::attach`.
    pub fn on_data_proxy(&mut self, py: Python<'_>, proxy: &SliceProxy, slice: &Slice) {
        proxy.update(py, slice);
        self.update_indicators(py, slice);
        if let Err(e) = self
            .py_obj
            .call_method1(py, "OnData", (proxy.py_slice.bind(py),))
        {
            e.print(py);
        }
    }

    /// Update all registered indicators with bars from the current slice.
    fn update_indicators(&self, py: Python<'_>, slice: &Slice) {
        let registry = self.indicators.lock().unwrap();
        for (sid, indicator) in &registry.entries {
            if let Some(bar) = slice.bars.get(sid) {
                let py_bar = PyTradeBar::from(bar);
                if let Ok(py_bar_obj) = pyo3::Py::new(py, py_bar) {
                    let _ = indicator.call_method1(py, "update_bar", (py_bar_obj,));
                }
            }
        }
    }

    /// Build an adapter from a Python strategy instance.
    /// The instance must be a subclass of `lean_rust.QcAlgorithm`.
    pub fn from_instance(py: Python<'_>, instance: Py<PyAny>) -> PyResult<Self> {
        let bound = instance.bind(py);
        let qc_ref = bound.cast::<PyQcAlgorithm>()?;
        let inner = qc_ref.borrow().inner_arc();
        let charts = qc_ref.borrow().charts_arc();
        let framework = qc_ref.borrow().framework_arc();
        let indicators = qc_ref.borrow().indicators_arc();
        let universes = qc_ref.borrow().universes_arc();
        let name = inner.lock().unwrap().name.clone();
        Ok(PyAlgorithmAdapter {
            py_obj: instance,
            inner,
            charts,
            framework,
            indicators,
            universes,
            name,
        })
    }

    /// Apply all due universes at the current frontier and add subscriptions
    /// for selected symbols, mirroring C# LEAN's UniverseSelection/DataManager flow.
    pub fn apply_universe_selection(
        &mut self,
        py: Python<'_>,
        utc_ns: i64,
        resolution: lean_core::Resolution,
    ) -> SecurityChanges {
        let universes = self.universes.lock().unwrap();
        let mut merged = SecurityChanges::empty();
        for universe in universes.iter() {
            let mut bound = universe.bind(py).borrow_mut();
            if !bound.should_trigger(utc_ns, resolution) {
                continue;
            }
            match bound.select(py, utc_ns) {
                Ok(changes) => {
                    if changes.has_changes() {
                        let settings = bound.settings();
                        {
                            let mut alg = self.inner.lock().unwrap();
                            for symbol in &changes.added {
                                alg.add_equity(&symbol.value, settings.resolution);
                            }
                        }
                        merged.added.extend(changes.added);
                        merged.removed.extend(changes.removed);
                    }
                }
                Err(e) => e.print(py),
            }
        }
        drop(universes);
        if merged.has_changes() {
            self.on_securities_changed(&merged);
        }
        merged
    }
}

impl IAlgorithm for PyAlgorithmAdapter {
    fn name(&self) -> &str {
        &self.name
    }

    fn start_date(&self) -> DateTime {
        self.inner.lock().unwrap().start_date
    }

    fn end_date(&self) -> DateTime {
        self.inner.lock().unwrap().end_date
    }

    fn status(&self) -> AlgorithmStatus {
        self.inner.lock().unwrap().status
    }

    fn initialize(&mut self) -> LeanResult<()> {
        Python::attach(|py| {
            self.py_obj.call_method0(py, "Initialize").map_err(|e| {
                e.print(py);
                anyhow::anyhow!("Python Initialize() failed")
            })?;
            self.name = self.inner.lock().unwrap().name.clone();
            Ok(())
        })
    }

    fn on_data(&mut self, slice: &Slice) {
        Python::attach(|py| {
            self.update_indicators(py, slice);
            match PySlice::from_slice(py, slice) {
                Ok(py_slice) => {
                    if let Err(e) = self.py_obj.call_method1(py, "OnData", (py_slice,)) {
                        e.print(py);
                    }
                }
                Err(e) => e.print(py),
            }
        });
    }

    fn on_order_event(&mut self, event: &OrderEvent) {
        Python::attach(|py| {
            let py_event = PyOrderEvent::from(event);
            if let Err(e) = self.py_obj.call_method1(py, "OnOrderEvent", (py_event,)) {
                if !e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                    e.print(py);
                }
            }
        });
    }

    fn on_end_of_day(&mut self, _symbol: Option<Symbol>) {
        Python::attach(|py| {
            lean_call0(py, &self.py_obj, "OnEndOfDay");
        });
    }

    fn on_end_of_algorithm(&mut self) {
        Python::attach(|py| {
            lean_call0(py, &self.py_obj, "OnEndOfAlgorithm");
        });
    }

    fn on_warmup_finished(&mut self) {
        Python::attach(|py| {
            lean_call0(py, &self.py_obj, "OnWarmupFinished");
        });
    }

    fn on_margin_call(&mut self, _requests: &[Order]) {
        Python::attach(|py| {
            lean_call0(py, &self.py_obj, "OnMarginCall");
        });
    }

    fn on_securities_changed(&mut self, changes: &SecurityChanges) {
        Python::attach(|py| {
            let py_changes = PySecurityChanges::from_changes(changes);
            if let Ok(changes_obj) = Py::new(py, py_changes) {
                let changes_for_pascal = changes_obj.clone_ref(py);
                if let Err(e) =
                    self.py_obj
                        .call_method1(py, "OnSecuritiesChanged", (changes_for_pascal,))
                {
                    if e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                        let _ =
                            self.py_obj
                                .call_method1(py, "on_securities_changed", (changes_obj,));
                    } else {
                        e.print(py);
                    }
                }
            }
        });
    }

    fn portfolio_value(&self) -> lean_core::Price {
        self.inner.lock().unwrap().portfolio_value()
    }

    fn starting_cash(&self) -> lean_core::Price {
        self.inner.lock().unwrap().cash()
    }
}

impl PyAlgorithmAdapter {
    pub fn on_otm_expiry(
        &self,
        contract: lean_options::OptionContract,
        quantity: rust_decimal::Decimal,
        underlying_price: rust_decimal::Decimal,
        entry_premium: rust_decimal::Decimal,
    ) {
        use rust_decimal::prelude::ToPrimitive;
        let utc_ns = self.inner.lock().unwrap().time.0;
        let event = crate::py_orders::PyOrderEvent::for_otm_expiry_event(
            contract.symbol,
            utc_ns,
            quantity,
            underlying_price.to_f64().unwrap_or(0.0),
            (entry_premium * quantity * rust_decimal_macros::dec!(100))
                .to_f64()
                .unwrap_or(0.0),
        );
        Python::attach(|py| {
            if let Err(e) = self.py_obj.call_method1(py, "OnOrderEvent", (event,)) {
                if !e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                    tracing::warn!("OnOrderEvent (OTM expiry) error: {e}");
                }
            }
        });
    }

    pub fn on_delistings(
        &self,
        py: Python<'_>,
        delistings: pyo3::Py<crate::py_data::PyDelistings>,
    ) {
        if let Err(e) = self.py_obj.call_method1(py, "OnDelistings", (delistings,)) {
            if !e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                tracing::warn!("OnDelistings error: {e}");
            }
        }
    }

    pub fn on_symbol_changed_events(
        &self,
        py: Python<'_>,
        events: pyo3::Py<crate::py_data::PySymbolChangedEvents>,
    ) {
        if let Err(e) = self
            .py_obj
            .call_method1(py, "OnSymbolChangedEvents", (events,))
        {
            if !e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                tracing::warn!("OnSymbolChangedEvents error: {e}");
            }
        }
    }

    pub fn on_assignment_order_event(
        &self,
        contract: lean_options::OptionContract,
        quantity: rust_decimal::Decimal,
        is_assignment: bool,
    ) {
        let utc_ns = self.inner.lock().unwrap().time.0;
        let event = crate::py_orders::PyOrderEvent::for_expiry_event(
            contract.symbol,
            utc_ns,
            quantity,
            is_assignment,
        );
        Python::attach(|py| {
            if let Err(e) = self
                .py_obj
                .call_method1(py, "OnAssignmentOrderEvent", (event,))
            {
                if !e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                    tracing::warn!("OnAssignmentOrderEvent error: {e}");
                }
            }
        });
    }
}

// ─── Lifecycle dispatch helpers ───────────────────────────────────────────────

/// Call `method` on `obj`.  AttributeError (method not defined — optional hook)
/// is silently ignored; other errors are printed.
fn lean_call0(py: Python<'_>, obj: &Py<PyAny>, method: &str) {
    if let Err(e) = obj.call_method0(py, method) {
        if !e.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
            e.print(py);
        }
    }
}

/// Update the algorithm's current time so orders use the right timestamp.
pub fn set_algorithm_time(adapter: &PyAlgorithmAdapter, time: DateTime) {
    let mut inner = adapter.inner.lock().unwrap();
    inner.utc_time = time;
    inner.time = time;
}
