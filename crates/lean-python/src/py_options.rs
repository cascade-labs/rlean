use pyo3::prelude::*;
use rust_decimal::prelude::ToPrimitive;
use lean_core::{OptionRight, OptionStyle};
use lean_options::{OptionContract, OptionChain};

// ─── PyOptionRight ────────────────────────────────────────────────────────────

#[pyclass(name = "OptionRight")]
#[derive(Clone, Copy, Debug)]
pub struct PyOptionRight {
    pub inner: OptionRight,
}

#[pymethods]
impl PyOptionRight {
    #[classattr]
    fn call_() -> Self { Self { inner: OptionRight::Call } }
    #[classattr]
    fn put() -> Self { Self { inner: OptionRight::Put } }
    fn __repr__(&self) -> &'static str {
        match self.inner { OptionRight::Call => "OptionRight.Call", OptionRight::Put => "OptionRight.Put" }
    }
    fn is_call(&self) -> bool { self.inner == OptionRight::Call }
    fn is_put(&self) -> bool { self.inner == OptionRight::Put }
}

// ─── PyGreeks ─────────────────────────────────────────────────────────────────

#[pyclass(name = "Greeks")]
#[derive(Clone, Debug, Default)]
pub struct PyGreeks {
    #[pyo3(get)] pub delta: f64,
    #[pyo3(get)] pub gamma: f64,
    #[pyo3(get)] pub vega: f64,
    #[pyo3(get)] pub theta: f64,
    #[pyo3(get)] pub rho: f64,
    #[pyo3(get)] pub lambda: f64,
}

#[pymethods]
impl PyGreeks {
    fn theta_per_day(&self) -> f64 { self.theta / 365.0 }
    fn __repr__(&self) -> String {
        format!("Greeks(delta={:.4}, gamma={:.4}, vega={:.4}, theta={:.4})", self.delta, self.gamma, self.vega, self.theta)
    }
}

impl From<lean_core::Greeks> for PyGreeks {
    fn from(g: lean_core::Greeks) -> Self {
        let to_f = |d: rust_decimal::Decimal| d.to_f64().unwrap_or(0.0);
        Self {
            delta: to_f(g.delta),
            gamma: to_f(g.gamma),
            vega: to_f(g.vega),
            theta: to_f(g.theta),
            rho: to_f(g.rho),
            lambda: to_f(g.lambda),
        }
    }
}

// ─── PyOptionContract ─────────────────────────────────────────────────────────

#[pyclass(name = "OptionContract")]
#[derive(Clone, Debug)]
pub struct PyOptionContract {
    pub inner: OptionContract,
}

#[pymethods]
impl PyOptionContract {
    #[getter] fn strike(&self) -> f64 { self.inner.strike.to_f64().unwrap_or(0.0) }
    #[getter] fn expiry(&self) -> String { self.inner.expiry.to_string() }
    #[getter] fn right(&self) -> PyOptionRight { PyOptionRight { inner: self.inner.right } }
    #[getter] fn style(&self) -> String {
        match self.inner.style {
            OptionStyle::American => "American".to_string(),
            OptionStyle::European => "European".to_string(),
        }
    }
    #[getter] fn underlying_price(&self) -> f64 {
        self.inner.data.underlying_last_price.to_f64().unwrap_or(0.0)
    }
    #[getter] fn implied_volatility(&self) -> f64 {
        self.inner.data.implied_volatility.to_f64().unwrap_or(0.0)
    }
    #[getter] fn open_interest(&self) -> f64 {
        self.inner.data.open_interest.to_f64().unwrap_or(0.0)
    }
    #[getter] fn greeks(&self) -> PyGreeks {
        PyGreeks::from(self.inner.data.greeks.clone())
    }
    #[getter] fn last_price(&self) -> f64 { self.inner.data.last_price.to_f64().unwrap_or(0.0) }
    #[getter] fn bid_price(&self) -> f64 { self.inner.data.bid_price.to_f64().unwrap_or(0.0) }
    #[getter] fn ask_price(&self) -> f64 { self.inner.data.ask_price.to_f64().unwrap_or(0.0) }
    #[getter] fn mid_price(&self) -> f64 { self.inner.mid_price().to_f64().unwrap_or(0.0) }
    #[getter] fn volume(&self) -> i64 { self.inner.data.volume }
    #[getter] fn ticker(&self) -> String { self.inner.symbol.permtick.clone() }
    fn is_call(&self) -> bool { self.inner.right == OptionRight::Call }
    fn is_put(&self) -> bool { self.inner.right == OptionRight::Put }
    fn intrinsic_value(&self) -> f64 {
        self.inner.intrinsic_value().to_f64().unwrap_or(0.0)
    }
    fn time_value(&self) -> f64 {
        self.inner.time_value().to_f64().unwrap_or(0.0)
    }
    fn __repr__(&self) -> String {
        format!(
            "OptionContract({} {} K={:.2} exp={})",
            if self.inner.right == OptionRight::Call { "Call" } else { "Put" },
            self.inner.symbol.permtick,
            self.inner.strike.to_f64().unwrap_or(0.0),
            self.inner.expiry
        )
    }
}

// ─── PyOptionChain ────────────────────────────────────────────────────────────

#[pyclass(name = "OptionChain")]
#[derive(Clone, Debug)]
pub struct PyOptionChain {
    pub inner: OptionChain,
}

#[pymethods]
impl PyOptionChain {
    #[getter]
    fn underlying(&self) -> String { self.inner.canonical_symbol.permtick.clone() }

    #[getter]
    fn underlying_price(&self) -> f64 {
        self.inner.underlying_price.to_f64().unwrap_or(0.0)
    }

    fn contracts(&self) -> Vec<PyOptionContract> {
        self.inner.contracts.values()
            .map(|c| PyOptionContract { inner: c.clone() })
            .collect()
    }

    fn calls(&self) -> Vec<PyOptionContract> {
        self.inner.contracts.values()
            .filter(|c| c.right == OptionRight::Call)
            .map(|c| PyOptionContract { inner: c.clone() })
            .collect()
    }

    fn puts(&self) -> Vec<PyOptionContract> {
        self.inner.contracts.values()
            .filter(|c| c.right == OptionRight::Put)
            .map(|c| PyOptionContract { inner: c.clone() })
            .collect()
    }

    fn filter(&self, py: Python, filter_fn: PyObject) -> PyResult<Vec<PyOptionContract>> {
        let all: Vec<Py<PyOptionContract>> = self.inner.contracts.values()
            .map(|c| Py::new(py, PyOptionContract { inner: c.clone() }))
            .collect::<PyResult<Vec<_>>>()?;
        let py_list = pyo3::types::PyList::new(py, &all)?;
        let filtered = filter_fn.call1(py, (py_list,))?;
        let result: Vec<PyOptionContract> = filtered.extract(py)?;
        Ok(result)
    }

    fn where_expiry(&self, min_days: i64, max_days: i64) -> Vec<PyOptionContract> {
        use chrono::Local;
        let today = Local::now().date_naive();
        self.inner.contracts.values()
            .filter(|c| {
                let days = (c.expiry - today).num_days();
                days >= min_days && days <= max_days
            })
            .map(|c| PyOptionContract { inner: c.clone() })
            .collect()
    }

    fn where_strike(&self, min_pct: f64, max_pct: f64) -> Vec<PyOptionContract> {
        use rust_decimal::Decimal;
        use rust_decimal::prelude::FromPrimitive;
        let spot = self.inner.underlying_price;
        if spot.is_zero() { return vec![]; }
        let lo = spot * Decimal::from_f64(1.0 + min_pct).unwrap_or(Decimal::ONE);
        let hi = spot * Decimal::from_f64(1.0 + max_pct).unwrap_or(Decimal::ONE);
        self.inner.contracts.values()
            .filter(|c| c.strike >= lo && c.strike <= hi)
            .map(|c| PyOptionContract { inner: c.clone() })
            .collect()
    }

    fn __len__(&self) -> usize { self.inner.contracts.len() }

    fn __repr__(&self) -> String {
        format!(
            "OptionChain({}, {} contracts, spot={:.2})",
            self.inner.canonical_symbol.permtick,
            self.inner.contracts.len(),
            self.inner.underlying_price.to_f64().unwrap_or(0.0),
        )
    }
}
