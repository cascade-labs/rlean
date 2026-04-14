use pyo3::prelude::*;
use lean_orders::OrderEvent;
use lean_orders::order::{OrderDirection, OrderStatus};
use rust_decimal::prelude::ToPrimitive;
use crate::py_types::PySymbol;
use crate::{PyOrderDirection, PyOrderStatus};

/// Python-visible OrderEvent — mirrors C# LEAN's `OrderEvent` 1:1.
///
/// All property names are snake_case equivalents of the C# PascalCase names.
/// `is_fill` is a computed bool (equivalent to C# `OrderStatus.IsFill()`).
#[pyclass(name = "OrderEvent", frozen)]
#[derive(Debug, Clone)]
pub struct PyOrderEvent {
    inner_utc_ns:          i64,
    pub order_id:          i64,
    pub id:                i64,
    pub symbol:            PySymbol,
    status_raw:            OrderStatus,
    direction_raw:         OrderDirection,
    pub fill_price:        f64,
    pub fill_price_currency: String,
    pub fill_quantity:     f64,
    pub quantity:          f64,
    pub is_assignment:     bool,
    pub is_in_the_money:   bool,
    pub message:           String,
    pub order_fee:         f64,
    pub limit_price:       Option<f64>,
    pub stop_price:        Option<f64>,
    pub trigger_price:     Option<f64>,
    pub trailing_amount:   Option<f64>,
    pub trailing_as_percentage: bool,
}

impl PyOrderEvent {
    /// Build a synthetic OTM-expiry event matching LEAN's `on_order_event` format.
    ///
    /// LEAN fires `on_order_event` (not `on_assignment_order_event`) when an
    /// option expires worthless.  The event has fill_price=0 and a message
    /// of the form "OTM. Underlying: X. Profit: Y".
    pub fn for_otm_expiry_event(
        symbol: lean_core::Symbol,
        utc_ns: i64,
        quantity: rust_decimal::Decimal,
        underlying_price: f64,
        profit: f64,
    ) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        PyOrderEvent {
            inner_utc_ns:          utc_ns,
            order_id:              -1,
            id:                    -1,
            symbol:                crate::py_types::PySymbol { inner: symbol },
            status_raw:            OrderStatus::Filled,
            direction_raw:         OrderDirection::Buy, // closing short
            fill_price:            0.0,
            fill_price_currency:   "USD".to_string(),
            fill_quantity:         quantity.to_f64().unwrap_or(0.0),
            quantity:              quantity.to_f64().unwrap_or(0.0),
            is_assignment:         false,
            is_in_the_money:       false,
            message:               format!("OTM. Underlying: {underlying_price:.4}. Profit: +{profit:.2}"),
            order_fee:             0.0,
            limit_price:           None,
            stop_price:            None,
            trigger_price:         None,
            trailing_amount:       None,
            trailing_as_percentage: false,
        }
    }

    /// Build a synthetic assignment/expiry event for `on_assignment_order_event`.
    pub fn for_expiry_event(
        symbol: lean_core::Symbol,
        utc_ns: i64,
        quantity: rust_decimal::Decimal,
        is_assignment: bool,
    ) -> Self {
        use rust_decimal::prelude::ToPrimitive;
        PyOrderEvent {
            inner_utc_ns:          utc_ns,
            order_id:              -1,
            id:                    -1,
            symbol:                crate::py_types::PySymbol { inner: symbol },
            status_raw:            OrderStatus::Filled,
            direction_raw:         OrderDirection::Hold,
            fill_price:            0.0,
            fill_price_currency:   "USD".to_string(),
            fill_quantity:         quantity.to_f64().unwrap_or(0.0),
            quantity:              quantity.to_f64().unwrap_or(0.0),
            is_assignment,
            is_in_the_money:       is_assignment,
            message:               if is_assignment { "Assignment".to_string() } else { "Expiry".to_string() },
            order_fee:             0.0,
            limit_price:           None,
            stop_price:            None,
            trigger_price:         None,
            trailing_amount:       None,
            trailing_as_percentage: false,
        }
    }
}

impl From<&OrderEvent> for PyOrderEvent {
    fn from(e: &OrderEvent) -> Self {
        PyOrderEvent {
            inner_utc_ns:         e.utc_time.0,
            order_id:             e.order_id,
            id:                   e.id,
            symbol:               PySymbol { inner: e.symbol.clone() },
            status_raw:           e.status,
            direction_raw:        e.direction,
            fill_price:           e.fill_price.to_f64().unwrap_or(0.0),
            fill_price_currency:  e.fill_price_currency.clone(),
            fill_quantity:        e.fill_quantity.to_f64().unwrap_or(0.0),
            quantity:             e.quantity.to_f64().unwrap_or(0.0),
            is_assignment:        e.is_assignment,
            is_in_the_money:      e.is_in_the_money,
            message:              e.message.clone(),
            order_fee:            e.order_fee.to_f64().unwrap_or(0.0),
            limit_price:          e.limit_price.and_then(|p| p.to_f64()),
            stop_price:           e.stop_price.and_then(|p| p.to_f64()),
            trigger_price:        e.trigger_price.and_then(|p| p.to_f64()),
            trailing_amount:      e.trailing_amount.and_then(|p| p.to_f64()),
            trailing_as_percentage: e.trailing_as_percentage,
        }
    }
}

#[pymethods]
impl PyOrderEvent {
    // ─── Primitive getters (avoid get_all so we can also expose computed props)

    #[getter]
    fn order_id(&self) -> i64 { self.order_id }

    /// Sequential event id — distinct from `order_id`.
    #[getter]
    fn id(&self) -> i64 { self.id }

    #[getter]
    fn symbol(&self) -> PySymbol { self.symbol.clone() }

    /// Event time in UTC as a `datetime.datetime`.
    #[getter]
    fn utc_time(&self, py: Python<'_>) -> PyResult<PyObject> {
        ns_to_py_datetime(py, self.inner_utc_ns)
    }

    /// Order status as a `OrderStatus` enum value.
    #[getter]
    fn status(&self) -> PyOrderStatus {
        match self.status_raw {
            OrderStatus::New             => PyOrderStatus::New,
            OrderStatus::Submitted       => PyOrderStatus::Submitted,
            OrderStatus::PartiallyFilled => PyOrderStatus::PartiallyFilled,
            OrderStatus::Filled          => PyOrderStatus::Filled,
            OrderStatus::Canceled        => PyOrderStatus::Canceled,
            OrderStatus::None            => PyOrderStatus::Invalid,
            OrderStatus::Invalid         => PyOrderStatus::Invalid,
            OrderStatus::CancelPending   => PyOrderStatus::CancelPending,
            OrderStatus::UpdateSubmitted => PyOrderStatus::UpdateSubmitted,
        }
    }

    /// Order direction as an `OrderDirection` enum value.
    #[getter]
    fn direction(&self) -> PyOrderDirection {
        match self.direction_raw {
            OrderDirection::Buy  => PyOrderDirection::Buy,
            OrderDirection::Sell => PyOrderDirection::Sell,
            OrderDirection::Hold => PyOrderDirection::Hold,
        }
    }

    #[getter]
    fn fill_price(&self) -> f64 { self.fill_price }

    #[getter]
    fn fill_price_currency(&self) -> &str { &self.fill_price_currency }

    #[getter]
    fn fill_quantity(&self) -> f64 { self.fill_quantity }

    /// `|fill_quantity|` — mirrors C# `AbsoluteFillQuantity`.
    #[getter]
    fn absolute_fill_quantity(&self) -> f64 { self.fill_quantity.abs() }

    #[getter]
    fn quantity(&self) -> f64 { self.quantity }

    #[getter]
    fn is_assignment(&self) -> bool { self.is_assignment }

    #[getter]
    fn is_in_the_money(&self) -> bool { self.is_in_the_money }

    #[getter]
    fn message(&self) -> &str { &self.message }

    /// True when `status` is `Filled` or `PartiallyFilled`.
    /// Equivalent to C# `order_event.Status.IsFill()`.
    #[getter]
    fn is_fill(&self) -> bool { self.status_raw.is_fill() }

    /// Brokerage commission for this fill.
    #[getter]
    fn order_fee(&self) -> f64 { self.order_fee }

    #[getter]
    fn limit_price(&self) -> Option<f64> { self.limit_price }

    #[getter]
    fn stop_price(&self) -> Option<f64> { self.stop_price }

    #[getter]
    fn trigger_price(&self) -> Option<f64> { self.trigger_price }

    #[getter]
    fn trailing_amount(&self) -> Option<f64> { self.trailing_amount }

    #[getter]
    fn trailing_as_percentage(&self) -> bool { self.trailing_as_percentage }

    fn __repr__(&self) -> String {
        format!(
            "OrderEvent(id={}, {} {} qty={:.0} @ {:.2} [{:?}])",
            self.order_id,
            self.symbol.inner.value,
            match self.direction_raw {
                OrderDirection::Buy  => "Buy",
                OrderDirection::Sell => "Sell",
                OrderDirection::Hold => "Hold",
            },
            self.fill_quantity,
            self.fill_price,
            self.status_raw,
        )
    }
}

fn ns_to_py_datetime(py: Python<'_>, ns: i64) -> PyResult<PyObject> {
    let secs = ns / 1_000_000_000;
    let micros = (ns % 1_000_000_000) / 1_000;
    let timestamp = secs as f64 + micros as f64 / 1_000_000.0;
    let datetime = py
        .import("datetime")?
        .getattr("datetime")?
        .call_method1("utcfromtimestamp", (timestamp,))?;
    Ok(datetime.into())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, NanosecondTimestamp, Symbol};
    use lean_orders::order::OrderStatus;
    use rust_decimal_macros::dec;

    fn make_filled_event() -> OrderEvent {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut ev = OrderEvent::filled(42, sym, NanosecondTimestamp(1_620_000_000_000_000_000), dec!(420.00), dec!(10));
        ev.id = 1;
        ev.order_fee = dec!(1.00);
        ev
    }

    #[test]
    fn test_py_order_event_is_fill() {
        let ev = make_filled_event();
        let py_ev = PyOrderEvent::from(&ev);
        assert!(py_ev.is_fill(), "Filled event must have is_fill == true");
    }

    #[test]
    fn test_py_order_event_fields() {
        let ev = make_filled_event();
        let py_ev = PyOrderEvent::from(&ev);

        assert_eq!(py_ev.order_id(), 42);
        assert_eq!(py_ev.id(), 1);
        assert!((py_ev.fill_price() - 420.0).abs() < 1e-9);
        assert!((py_ev.fill_quantity() - 10.0).abs() < 1e-9);
        assert!((py_ev.absolute_fill_quantity() - 10.0).abs() < 1e-9);
        assert!((py_ev.quantity() - 10.0).abs() < 1e-9);
        assert!((py_ev.order_fee() - 1.0).abs() < 1e-9);
        assert_eq!(py_ev.fill_price_currency(), "USD");
        assert_eq!(py_ev.message(), "Order filled");
        assert!(!py_ev.is_assignment());
        assert!(!py_ev.is_in_the_money());
        assert!(py_ev.limit_price().is_none());
        assert!(py_ev.stop_price().is_none());
    }

    #[test]
    fn test_py_order_event_not_fill_when_submitted() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let ev = OrderEvent::new(1, sym, NanosecondTimestamp(0), OrderStatus::Submitted);
        let py_ev = PyOrderEvent::from(&ev);
        assert!(!py_ev.is_fill(), "Submitted event must not be a fill");
    }

    #[test]
    fn test_py_order_event_partial_fill() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut ev = OrderEvent::new(2, sym, NanosecondTimestamp(0), OrderStatus::PartiallyFilled);
        ev.fill_quantity = dec!(5);
        let py_ev = PyOrderEvent::from(&ev);
        assert!(py_ev.is_fill(), "PartiallyFilled event must have is_fill == true");
        assert!((py_ev.absolute_fill_quantity() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_absolute_fill_quantity_negative_qty() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut ev = OrderEvent::filled(3, sym, NanosecondTimestamp(0), dec!(100), dec!(-5));
        ev.fill_quantity = dec!(-5);
        let py_ev = PyOrderEvent::from(&ev);
        assert!((py_ev.fill_quantity() - (-5.0)).abs() < 1e-9);
        assert!((py_ev.absolute_fill_quantity() - 5.0).abs() < 1e-9,
            "absolute_fill_quantity must be positive for sell orders");
    }

}
