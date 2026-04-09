use std::sync::{Arc, Mutex};
use pyo3::prelude::*;
use lean_algorithm::algorithm::{AlgorithmStatus, IAlgorithm, SecurityChanges};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{DateTime, Result as LeanResult, Symbol};
use lean_data::Slice;
use lean_orders::{Order, OrderEvent};
use crate::charting::ChartCollection;
use crate::py_data::PySlice;
use crate::py_orders::PyOrderEvent;
use crate::py_qc_algorithm::PyQcAlgorithm;

/// Bridges a Python strategy object to the Rust `IAlgorithm` trait.
/// Holds both the Python object (for calling `on_data` etc.) and
/// the shared QcAlgorithm state (for reading dates, portfolio, etc.).
pub struct PyAlgorithmAdapter {
    /// The Python strategy instance (subclass of QcAlgorithm).
    py_obj: Py<PyAny>,
    /// Shared inner state — same Arc as inside the Python QcAlgorithm instance.
    pub inner: Arc<Mutex<QcAlgorithm>>,
    /// Shared chart collection — populated by the Python strategy via self.plot().
    pub charts: Arc<Mutex<ChartCollection>>,
    /// Cached after initialize().
    pub name: String,
}

impl PyAlgorithmAdapter {
    /// Build an adapter from a Python strategy instance.
    /// The instance must be a subclass of `lean_rust.QcAlgorithm`.
    pub fn from_instance(py: Python<'_>, instance: Py<PyAny>) -> PyResult<Self> {
        // Downcast to PyQcAlgorithm to extract the inner Arc
        let bound = instance.bind(py);
        let qc_ref = bound.downcast::<PyQcAlgorithm>()?;
        let inner = qc_ref.borrow().inner_arc();
        let charts = qc_ref.borrow().charts_arc();
        let name = inner.lock().unwrap().name.clone();
        Ok(PyAlgorithmAdapter { py_obj: instance, inner, charts, name })
    }
}

impl IAlgorithm for PyAlgorithmAdapter {
    fn name(&self) -> &str { &self.name }

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
        Python::with_gil(|py| {
            self.py_obj.call_method0(py, "initialize")
                .map_err(|e| {
                    e.print(py);
                    anyhow::anyhow!("Python initialize() failed")
                })?;
            // Update cached name in case set_name was called
            self.name = self.inner.lock().unwrap().name.clone();
            Ok(())
        })
    }

    fn on_data(&mut self, slice: &Slice) {
        Python::with_gil(|py| {
            let py_slice = PySlice::from_slice(slice);
            if let Err(e) = self.py_obj.call_method1(py, "on_data", (py_slice,)) {
                e.print(py);
            }
        });
    }

    fn on_order_event(&mut self, event: &OrderEvent) {
        Python::with_gil(|py| {
            let py_event = PyOrderEvent::from(event);
            if let Err(e) = self.py_obj.call_method1(py, "on_order_event", (py_event,)) {
                e.print(py);
            }
        });
    }

    fn on_end_of_day(&mut self, _symbol: Option<Symbol>) {
        Python::with_gil(|py| {
            let _ = self.py_obj.call_method1(py, "on_end_of_day", (py.None(),));
        });
    }

    fn on_end_of_algorithm(&mut self) {
        Python::with_gil(|py| {
            if let Err(e) = self.py_obj.call_method0(py, "on_end_of_algorithm") {
                e.print(py);
            }
        });
    }

    fn on_warmup_finished(&mut self) {
        Python::with_gil(|py| {
            let _ = self.py_obj.call_method0(py, "on_warmup_finished");
        });
    }

    fn on_margin_call(&mut self, _requests: &[Order]) {
        Python::with_gil(|py| {
            let _ = self.py_obj.call_method0(py, "on_margin_call");
        });
    }

    fn on_securities_changed(&mut self, _changes: &SecurityChanges) {
        Python::with_gil(|py| {
            let _ = self.py_obj.call_method0(py, "on_securities_changed");
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
    /// Notify the Python strategy of an option assignment order event.
    pub fn on_assignment_order_event(
        &self,
        contract: lean_options::OptionContract,
        quantity: rust_decimal::Decimal,
        is_assignment: bool,
    ) {
        use rust_decimal::prelude::ToPrimitive;
        let result = Python::with_gil(|py| -> PyResult<()> {
            let contract_py = crate::py_options::PyOptionContract { inner: contract };
            self.py_obj.call_method1(
                py,
                "on_assignment_order_event",
                (contract_py, quantity.to_f64().unwrap_or(0.0), is_assignment),
            )?;
            Ok(())
        });
        if let Err(e) = result {
            tracing::warn!("on_assignment_order_event error: {e}");
        }
    }
}

/// Update the algorithm's current time so orders use the right timestamp.
pub fn set_algorithm_time(adapter: &PyAlgorithmAdapter, time: DateTime) {
    let mut inner = adapter.inner.lock().unwrap();
    inner.utc_time = time;
    inner.time = time;
}
