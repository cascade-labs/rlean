use std::sync::{Arc, Mutex};
use pyo3::prelude::*;
use lean_algorithm::algorithm::{AlgorithmStatus, IAlgorithm, SecurityChanges};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{DateTime, Result as LeanResult, Symbol};
use lean_data::Slice;
use lean_orders::{Order, OrderEvent};
use crate::charting::ChartCollection;
use crate::py_data::{PySlice, SliceProxy};
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
    /// Hot-path `on_data` using a pre-allocated `SliceProxy`.
    ///
    /// Updates the proxy's bar objects in-place (zero allocation), then calls
    /// Python's `on_data` with the same stable Python object as every other day.
    /// Must be called with the GIL already held via `Python::with_gil`.
    pub fn on_data_proxy(&mut self, py: Python<'_>, proxy: &SliceProxy, slice: &Slice) {
        proxy.update(py, slice);
        if let Err(e) = self.py_obj.call_method1(py, "on_data", (proxy.py_slice.bind(py),)) {
            e.print(py);
        }
    }

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
            match PySlice::from_slice(py, slice) {
                Ok(py_slice) => {
                    if let Err(e) = self.py_obj.call_method1(py, "on_data", (py_slice,)) {
                        e.print(py);
                    }
                }
                Err(e) => e.print(py),
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
    /// Notify the Python strategy of an option assignment/expiry event.
    ///
    /// LEAN API (OTM expiry): `on_order_event` with fill_price=0 and "OTM." message.
    ///
    /// LEAN fires `on_order_event` (not `on_assignment_order_event`) when an option
    /// expires worthless out-of-the-money.  This method constructs the matching event
    /// and dispatches to the Python `on_order_event` hook.
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
            (entry_premium * quantity * rust_decimal_macros::dec!(100)).to_f64().unwrap_or(0.0),
        );
        let result = Python::with_gil(|py| -> PyResult<()> {
            self.py_obj.call_method1(py, "on_order_event", (event,))?;
            Ok(())
        });
        if let Err(e) = result {
            tracing::warn!("on_otm_expiry (on_order_event) error: {e}");
        }
    }

    /// LEAN API: `on_assignment_order_event(order_event: OrderEvent)`
    /// where `order_event.is_assignment` distinguishes assignment vs worthless expiry.
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
        let result = Python::with_gil(|py| -> PyResult<()> {
            self.py_obj.call_method1(py, "on_assignment_order_event", (event,))?;
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
