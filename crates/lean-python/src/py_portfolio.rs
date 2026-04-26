use crate::py_types::{PySecurity, PySymbol};
use lean_algorithm::portfolio::{SecurityHolding, SecurityPortfolioManager};
use pyo3::prelude::*;
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;

/// Python-visible security holding (position in one symbol).
#[pyclass(name = "SecurityHolding", frozen, get_all)]
#[derive(Debug, Clone)]
pub struct PySecurityHolding {
    pub symbol: PySymbol,
    pub quantity: f64,
    pub average_price: f64,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
    pub total_fees: f64,
    pub last_price: f64,
    pub is_long: bool,
    pub is_short: bool,
    pub is_invested: bool,
    pub market_value: f64,
}

impl From<&SecurityHolding> for PySecurityHolding {
    fn from(h: &SecurityHolding) -> Self {
        PySecurityHolding {
            symbol: PySymbol {
                inner: h.symbol.clone(),
            },
            quantity: h.quantity.to_f64().unwrap_or(0.0),
            average_price: h.average_price.to_f64().unwrap_or(0.0),
            unrealized_pnl: h.unrealized_pnl.to_f64().unwrap_or(0.0),
            realized_pnl: h.realized_pnl.to_f64().unwrap_or(0.0),
            total_fees: h.total_fees.to_f64().unwrap_or(0.0),
            last_price: h.last_price.to_f64().unwrap_or(0.0),
            is_long: h.is_long(),
            is_short: h.is_short(),
            is_invested: h.is_invested(),
            market_value: h.market_value().to_f64().unwrap_or(0.0),
        }
    }
}

#[pymethods]
impl PySecurityHolding {
    /// LEAN API alias: `holding.invested`
    #[getter]
    fn invested(&self) -> bool {
        self.is_invested
    }

    /// LEAN API: `holding.unrealized_profit`
    #[getter]
    fn unrealized_profit(&self) -> f64 {
        self.unrealized_pnl
    }

    /// LEAN API: `holding.profit` — total of realized + unrealized P&L.
    #[getter]
    fn profit(&self) -> f64 {
        self.unrealized_pnl + self.realized_pnl
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'SecurityHolding' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "Holding({} qty={:.0} avg={:.2} pnl={:.2})",
            self.symbol.inner.value, self.quantity, self.average_price, self.unrealized_pnl
        )
    }
}

/// Python-visible portfolio manager.
#[pyclass(name = "Portfolio")]
pub struct PyPortfolio {
    pub inner: Arc<SecurityPortfolioManager>,
}

#[pymethods]
impl PyPortfolio {
    #[getter]
    fn cash(&self) -> f64 {
        self.inner.cash.read().to_f64().unwrap_or(0.0)
    }

    /// LEAN API: `portfolio.total_portfolio_value`
    #[getter]
    fn total_portfolio_value(&self) -> f64 {
        self.inner.total_portfolio_value().to_f64().unwrap_or(0.0)
    }

    /// Alias kept for compatibility.
    #[getter]
    fn total_value(&self) -> f64 {
        self.inner.total_portfolio_value().to_f64().unwrap_or(0.0)
    }

    #[getter]
    fn unrealized_pnl(&self) -> f64 {
        self.inner.unrealized_profit().to_f64().unwrap_or(0.0)
    }

    #[getter]
    fn total_holdings_value(&self) -> f64 {
        self.inner.total_holdings_value().to_f64().unwrap_or(0.0)
    }

    /// Compatibility spelling for whole-portfolio investment state.
    #[getter]
    fn is_invested(&self) -> bool {
        !self.inner.invested_symbols().is_empty()
    }

    /// LEAN API: `portfolio.invested`.
    #[getter]
    fn invested(&self) -> bool {
        self.is_invested()
    }

    /// LEAN API alias: `portfolio.hold_stock`.
    #[getter]
    fn hold_stock(&self) -> bool {
        self.is_invested()
    }

    /// Check if a specific symbol is invested.
    fn is_invested_in(&self, symbol: &PySymbol) -> bool {
        self.inner.is_invested(&symbol.inner)
    }

    /// Get the holding for a symbol (method form).
    fn get_holding(&self, symbol: &PySymbol) -> PySecurityHolding {
        PySecurityHolding::from(&self.inner.get_holding(&symbol.inner))
    }

    /// LEAN API: `portfolio[symbol]` — supports Symbol object or ticker string.
    fn __getitem__(&self, symbol: &Bound<'_, PyAny>) -> PyResult<PySecurityHolding> {
        let sym = self.resolve_symbol(symbol)?;
        Ok(PySecurityHolding::from(&self.inner.get_holding(&sym.inner)))
    }

    /// All current holdings as a list.
    fn holdings(&self) -> Vec<PySecurityHolding> {
        self.inner
            .all_holdings()
            .iter()
            .map(PySecurityHolding::from)
            .collect()
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Portfolio' object has no attribute '{name}'"
        )))
    }

    fn __repr__(&self) -> String {
        format!(
            "Portfolio(value={:.2}, cash={:.2}, invested={})",
            self.total_portfolio_value(),
            self.cash(),
            self.is_invested()
        )
    }
}

impl PyPortfolio {
    fn resolve_symbol(&self, arg: &Bound<'_, PyAny>) -> PyResult<PySymbol> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(sym.get().clone());
        }
        if let Ok(sec) = arg.cast::<PySecurity>() {
            return Ok(sec.get().inner.clone());
        }
        if let Ok(ticker) = arg.extract::<String>() {
            use lean_core::Market;
            let inner = lean_core::Symbol::create_equity(&ticker, &Market::usa());
            return Ok(PySymbol { inner });
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Security, Symbol, or ticker string",
        ))
    }
}
