//! SDK-owned portfolio view and mutation APIs.
//!
//! All decimal->f64 projection lives here so language bindings are pure
//! structural glue a generator can emit 1:1 from these getters.

use lean_algorithm::portfolio::{SecurityHolding, SecurityPortfolioManager};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{Market, Price, Symbol};
use rust_decimal::prelude::ToPrimitive;
use std::sync::Arc;
use std::sync::Mutex;

/// SDK-owned projection of a single `SecurityHolding`.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "SecurityHolding"))]
pub struct SecurityHoldingView {
    holding: SecurityHolding,
}

impl SecurityHoldingView {
    pub fn new(holding: SecurityHolding) -> Self {
        Self { holding }
    }
    pub fn symbol(&self) -> &Symbol {
        &self.holding.symbol
    }
    pub fn quantity(&self) -> f64 {
        self.holding.quantity.to_f64().unwrap_or(0.0)
    }
    pub fn average_price(&self) -> f64 {
        self.holding.average_price.to_f64().unwrap_or(0.0)
    }
    pub fn unrealized_pnl(&self) -> f64 {
        self.holding.unrealized_pnl.to_f64().unwrap_or(0.0)
    }
    /// LEAN alias for `unrealized_pnl`.
    pub fn unrealized_profit(&self) -> f64 {
        self.unrealized_pnl()
    }
    pub fn realized_pnl(&self) -> f64 {
        self.holding.realized_pnl.to_f64().unwrap_or(0.0)
    }
    /// realized + unrealized P&L — mirrors LEAN `holding.profit`.
    pub fn profit(&self) -> f64 {
        self.unrealized_pnl() + self.realized_pnl()
    }
    pub fn total_fees(&self) -> f64 {
        self.holding.total_fees.to_f64().unwrap_or(0.0)
    }
    pub fn total_funding(&self) -> f64 {
        self.holding.total_funding.to_f64().unwrap_or(0.0)
    }
    pub fn last_price(&self) -> f64 {
        self.holding.last_price.to_f64().unwrap_or(0.0)
    }
    pub fn is_long(&self) -> bool {
        self.holding.is_long()
    }
    pub fn is_short(&self) -> bool {
        self.holding.is_short()
    }
    pub fn is_invested(&self) -> bool {
        self.holding.is_invested()
    }
    /// LEAN alias for `is_invested`.
    pub fn invested(&self) -> bool {
        self.is_invested()
    }
    pub fn market_value(&self) -> f64 {
        self.holding.market_value().to_f64().unwrap_or(0.0)
    }

    pub fn display(&self) -> String {
        format!(
            "Holding({} qty={:.0} avg={:.2} pnl={:.2})",
            self.symbol().value,
            self.quantity(),
            self.average_price(),
            self.unrealized_pnl()
        )
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl SecurityHoldingView {
    #[getter(invested)]
    fn py_invested(&self) -> bool {
        self.invested()
    }
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "Portfolio"))]
pub struct PortfolioView {
    manager: Arc<SecurityPortfolioManager>,
    algorithm: Option<Arc<Mutex<QcAlgorithm>>>,
}

impl PortfolioView {
    pub fn new(manager: Arc<SecurityPortfolioManager>) -> Self {
        Self {
            manager,
            algorithm: None,
        }
    }

    pub fn from_algorithm(algorithm: Arc<Mutex<QcAlgorithm>>) -> Self {
        let manager = algorithm.lock().unwrap().portfolio.clone();
        Self {
            manager,
            algorithm: Some(algorithm),
        }
    }

    pub fn cash_decimal(&self) -> Price {
        *self.manager.cash.read()
    }
    pub fn cash_f64(&self) -> f64 {
        self.cash_decimal().to_f64().unwrap_or(0.0)
    }
    pub fn total_portfolio_value(&self) -> f64 {
        self.algorithm
            .as_ref()
            .map(|algorithm| algorithm.lock().unwrap().portfolio_value())
            .unwrap_or_else(|| self.manager.total_portfolio_value())
            .to_f64()
            .unwrap_or(0.0)
    }
    pub fn total_value(&self) -> f64 {
        self.total_portfolio_value()
    }
    pub fn unrealized_pnl(&self) -> f64 {
        self.algorithm
            .as_ref()
            .map(|algorithm| algorithm.lock().unwrap().unrealized_profit())
            .unwrap_or_else(|| self.manager.unrealized_profit())
            .to_f64()
            .unwrap_or(0.0)
    }
    pub fn total_holdings_value(&self) -> f64 {
        self.algorithm
            .as_ref()
            .map(|algorithm| algorithm.lock().unwrap().total_holdings_value())
            .unwrap_or_else(|| self.manager.total_holdings_value())
            .to_f64()
            .unwrap_or(0.0)
    }
    pub fn total_fees(&self) -> f64 {
        self.manager.total_fees.read().to_f64().unwrap_or(0.0)
    }
    pub fn total_funding(&self) -> f64 {
        self.manager.total_funding.read().to_f64().unwrap_or(0.0)
    }

    /// True when any symbol is invested.
    pub fn is_invested_any(&self) -> bool {
        !self.manager.invested_symbols().is_empty()
    }

    /// LEAN alias for whole-portfolio investment state.
    pub fn invested(&self) -> bool {
        self.is_invested_any()
    }

    /// LEAN alias for whole-portfolio investment state.
    pub fn hold_stock(&self) -> bool {
        self.is_invested_any()
    }

    pub fn is_invested_in(&self, symbol: &Symbol) -> bool {
        self.manager.is_invested(symbol)
    }

    pub fn default_equity_symbol(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    pub fn holding_for_ticker(&self, ticker: &str) -> SecurityHoldingView {
        self.holding(&Self::default_equity_symbol(ticker))
    }

    pub fn holding(&self, symbol: &Symbol) -> SecurityHoldingView {
        SecurityHoldingView::new(self.manager.get_holding(symbol))
    }

    pub fn __getitem__(&self, symbol: &Symbol) -> SecurityHoldingView {
        self.holding(symbol)
    }

    pub fn holdings(&self) -> Vec<SecurityHoldingView> {
        self.manager
            .all_holdings()
            .into_iter()
            .map(SecurityHoldingView::new)
            .collect()
    }

    pub fn display(&self) -> String {
        format!(
            "Portfolio(value={:.2}, cash={:.2}, invested={})",
            self.total_portfolio_value(),
            self.cash_f64(),
            self.is_invested_any()
        )
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl PortfolioView {
    #[getter(cash)]
    fn py_cash(&self) -> f64 {
        self.cash_f64()
    }

    #[getter(total_portfolio_value)]
    fn py_total_portfolio_value(&self) -> f64 {
        self.total_portfolio_value()
    }

    #[getter(hold_stock)]
    fn py_hold_stock(&self) -> bool {
        self.hold_stock()
    }

    #[pyo3(name = "__getitem__")]
    fn py_getitem(&self, symbol: crate::securities::SymbolHandle) -> SecurityHoldingView {
        self.holding(symbol.inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_algorithm::portfolio::SecurityHolding;
    use lean_core::Market;
    use rust_decimal_macros::dec;

    fn holding() -> SecurityHolding {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let mut h = SecurityHolding::new(sym);
        h.quantity = dec!(10);
        h.average_price = dec!(400);
        h.last_price = dec!(420);
        h.unrealized_pnl = dec!(200);
        h.realized_pnl = dec!(50);
        h
    }

    #[test]
    fn holding_view_projects_decimals() {
        let v = SecurityHoldingView::new(holding());
        assert!((v.quantity() - 10.0).abs() < 1e-9);
        assert!((v.average_price() - 400.0).abs() < 1e-9);
        assert!((v.last_price() - 420.0).abs() < 1e-9);
        assert!((v.market_value() - 4200.0).abs() < 1e-9);
    }

    #[test]
    fn holding_view_direction_and_profit() {
        let v = SecurityHoldingView::new(holding());
        assert!(v.is_long());
        assert!(!v.is_short());
        assert!(v.is_invested());
        assert!(v.invested());
        assert!((v.profit() - 250.0).abs() < 1e-9);
        assert!((v.unrealized_profit() - 200.0).abs() < 1e-9);
    }

    #[test]
    fn holding_view_flat_position_not_invested() {
        let sym = Symbol::create_equity("AAPL", &Market::usa());
        let v = SecurityHoldingView::new(SecurityHolding::new(sym));
        assert!(!v.is_invested());
        assert!(!v.is_long());
        assert!(!v.is_short());
    }

    #[test]
    fn portfolio_view_owns_lean_aliases_and_ticker_resolution() {
        let manager = Arc::new(SecurityPortfolioManager::new(dec!(1000)));
        let view = PortfolioView::new(manager);

        assert!((view.total_value() - view.total_portfolio_value()).abs() < 1e-9);
        assert!(!view.invested());
        assert!(!view.hold_stock());

        let symbol = PortfolioView::default_equity_symbol("spy");
        assert_eq!(symbol.value.as_ref(), "SPY");
        assert_eq!(
            view.holding_for_ticker("spy").symbol(),
            view.holding(&symbol).symbol()
        );
    }

    #[test]
    fn display_strings_are_sdk_projection_logic() {
        let holding = SecurityHoldingView::new(holding());
        assert_eq!(
            holding.display(),
            "Holding(SPY qty=10 avg=400.00 pnl=200.00)"
        );

        let manager = Arc::new(SecurityPortfolioManager::new(dec!(1000)));
        let portfolio = PortfolioView::new(manager);
        assert_eq!(
            portfolio.display(),
            "Portfolio(value=1000.00, cash=1000.00, invested=false)"
        );
    }
}
