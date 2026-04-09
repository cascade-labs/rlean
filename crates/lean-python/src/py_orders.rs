use pyo3::prelude::*;
use lean_orders::OrderEvent;
use rust_decimal::prelude::ToPrimitive;
use crate::py_types::PySymbol;

/// Python-visible OrderEvent.
#[pyclass(name = "OrderEvent", frozen, get_all)]
#[derive(Debug, Clone)]
pub struct PyOrderEvent {
    pub order_id:       i64,
    pub symbol:         PySymbol,
    pub fill_price:     f64,
    pub fill_quantity:  f64,
    pub is_fill:        bool,
    pub message:        String,
}

impl From<&OrderEvent> for PyOrderEvent {
    fn from(e: &OrderEvent) -> Self {
        PyOrderEvent {
            order_id:      e.order_id,
            symbol:        PySymbol { inner: e.symbol.clone() },
            fill_price:    e.fill_price.to_f64().unwrap_or(0.0),
            fill_quantity: e.fill_quantity.to_f64().unwrap_or(0.0),
            is_fill:       e.is_fill(),
            message:       e.message.clone(),
        }
    }
}

#[pymethods]
impl PyOrderEvent {
    fn __repr__(&self) -> String {
        format!(
            "OrderEvent(id={}, {} qty={:.0} @ {:.2})",
            self.order_id, self.symbol.inner.value,
            self.fill_quantity, self.fill_price
        )
    }
}
