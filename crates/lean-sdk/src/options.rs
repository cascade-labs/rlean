use chrono::{Local, NaiveDate, NaiveDateTime};
use lean_core::{DateTime, Greeks, OptionRight, OptionStyle, Symbol};
use lean_options::{implied_volatility, time_to_expiry_years, OptionChain, OptionContract};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "Greeks"))]
pub struct GreeksView {
    pub delta: f64,
    pub gamma: f64,
    pub vega: f64,
    pub theta: f64,
    pub rho: f64,
    pub lambda: f64,
}

impl GreeksView {
    pub fn delta(&self) -> f64 {
        self.delta
    }
    pub fn gamma(&self) -> f64 {
        self.gamma
    }
    pub fn vega(&self) -> f64 {
        self.vega
    }
    pub fn theta(&self) -> f64 {
        self.theta
    }

    pub fn theta_per_day(self) -> f64 {
        self.theta / 365.0
    }
}

impl Default for GreeksView {
    fn default() -> Self {
        Self::from(&Greeks::default())
    }
}

impl From<&Greeks> for GreeksView {
    fn from(greeks: &Greeks) -> Self {
        Self {
            delta: decimal_to_f64(greeks.delta),
            gamma: decimal_to_f64(greeks.gamma),
            vega: decimal_to_f64(greeks.vega),
            theta: decimal_to_f64(greeks.theta),
            rho: decimal_to_f64(greeks.rho),
            lambda: decimal_to_f64(greeks.lambda),
        }
    }
}

impl From<Greeks> for GreeksView {
    fn from(greeks: Greeks) -> Self {
        Self::from(&greeks)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionStyleView {
    American,
    European,
}

impl OptionStyleView {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::American => "American",
            Self::European => "European",
        }
    }
}

impl From<OptionStyle> for OptionStyleView {
    fn from(style: OptionStyle) -> Self {
        match style {
            OptionStyle::American => Self::American,
            OptionStyle::European => Self::European,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "OptionUnderlying"))]
pub struct UnderlyingView {
    pub price: f64,
    pub close: f64,
}

impl UnderlyingView {
    pub fn price(&self) -> f64 {
        self.price
    }
    pub fn close(&self) -> f64 {
        self.close
    }
}

impl From<&OptionChain> for UnderlyingView {
    fn from(chain: &OptionChain) -> Self {
        let price = decimal_to_f64(chain.underlying_price);
        Self {
            price,
            close: price,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "OptionContract"))]
pub struct OptionContractView {
    pub strike: f64,
    pub expiry: NaiveDateTime,
    pub right: OptionRight,
    pub style: OptionStyleView,
    pub underlying_price: f64,
    pub theoretical_price: f64,
    pub implied_volatility: f64,
    pub open_interest: f64,
    pub greeks: GreeksView,
    pub last_price: f64,
    pub bid_price: f64,
    pub ask_price: f64,
    pub mid_price: f64,
    pub volume: i64,
    pub ticker: String,
    pub symbol: Symbol,
    pub intrinsic_value: f64,
    pub time_value: f64,
}

impl OptionContractView {
    pub fn strike(&self) -> f64 {
        self.strike
    }
    pub fn expiry(&self) -> NaiveDateTime {
        self.expiry
    }
    pub fn right(&self) -> OptionRight {
        self.right
    }
    pub fn greeks(&self) -> GreeksView {
        self.greeks
    }
    pub fn bid_price(&self) -> f64 {
        self.bid_price
    }
    pub fn ask_price(&self) -> f64 {
        self.ask_price
    }
    pub fn symbol(&self) -> Symbol {
        self.symbol.clone()
    }

    /// Underlying equity/index symbol for this option contract.
    /// Mirrors LEAN `OptionContract.UnderlyingSymbol` (`Symbol.Underlying`).
    pub fn underlying_symbol(&self) -> Option<Symbol> {
        self.symbol.underlying().cloned()
    }

    pub fn is_call(&self) -> bool {
        self.right == OptionRight::Call
    }

    pub fn is_put(&self) -> bool {
        self.right == OptionRight::Put
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "OptionChain"))]
pub struct OptionChainView {
    contracts: Vec<OptionContractView>,
    underlying: UnderlyingView,
}

impl OptionChainView {
    pub fn from_chain(chain: &OptionChain) -> Self {
        Self {
            contracts: contract_views(chain),
            underlying: UnderlyingView::from(chain),
        }
    }
    pub fn contracts(&self) -> Vec<OptionContractView> {
        self.contracts.clone()
    }
    pub fn underlying(&self) -> UnderlyingView {
        self.underlying
    }

    pub fn __len__(&self) -> usize {
        self.contracts.len()
    }

    pub fn __iter__(&self) -> Vec<OptionContractView> {
        self.contracts.clone()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl OptionChainView {
    #[getter(contracts)]
    fn py_contracts(&self) -> Vec<OptionContractView> {
        OptionChainView::contracts(self)
    }

    #[getter(underlying)]
    fn py_underlying(&self) -> UnderlyingView {
        OptionChainView::underlying(self)
    }

    #[pyo3(name = "__len__")]
    fn py_len(&self) -> usize {
        OptionChainView::__len__(self)
    }

    #[pyo3(name = "__iter__")]
    fn py_iter<'py>(
        &self,
        py: pyo3::Python<'py>,
    ) -> pyo3::PyResult<pyo3::Bound<'py, pyo3::types::PyIterator>> {
        let list = pyo3::types::PyList::new(py, OptionChainView::contracts(self))?;
        pyo3::types::PyIterator::from_object(&list)
    }
}

impl From<&OptionContract> for OptionContractView {
    fn from(contract: &OptionContract) -> Self {
        Self {
            strike: decimal_to_f64(contract.strike),
            expiry: contract.expiry.and_hms_opt(0, 0, 0).unwrap_or_default(),
            right: contract.right,
            style: contract.style.into(),
            underlying_price: decimal_to_f64(contract.data.underlying_last_price),
            theoretical_price: decimal_to_f64(contract.data.theoretical_price),
            implied_volatility: decimal_to_f64(contract.data.implied_volatility),
            open_interest: decimal_to_f64(contract.data.open_interest),
            greeks: GreeksView::from(&contract.data.greeks),
            last_price: decimal_to_f64(contract.data.last_price),
            bid_price: decimal_to_f64(contract.data.bid_price),
            ask_price: decimal_to_f64(contract.data.ask_price),
            mid_price: decimal_to_f64(contract.mid_price()),
            volume: contract.data.volume,
            ticker: contract.symbol.permtick.to_string(),
            symbol: contract.symbol.clone(),
            intrinsic_value: decimal_to_f64(contract.intrinsic_value()),
            time_value: decimal_to_f64(contract.time_value()),
        }
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl OptionContractView {
    #[getter(symbol)]
    fn py_symbol(&self) -> crate::securities::SymbolHandle {
        crate::securities::SymbolHandle::new(self.symbol())
    }

    #[getter(underlying_symbol)]
    fn py_underlying_symbol(&self) -> Option<crate::securities::SymbolHandle> {
        self.underlying_symbol()
            .map(crate::securities::SymbolHandle::new)
    }

    #[getter(right)]
    fn py_right(&self) -> crate::types::OptionRight {
        self.right.into()
    }

    #[getter(style)]
    fn py_style(&self) -> crate::types::OptionStyle {
        match self.style {
            OptionStyleView::American => crate::types::OptionStyle::American,
            OptionStyleView::European => crate::types::OptionStyle::European,
        }
    }

    #[getter(strike)]
    fn py_strike(&self) -> f64 {
        self.strike
    }

    #[getter(expiry)]
    fn py_expiry(&self) -> NaiveDateTime {
        self.expiry
    }

    #[getter(underlying_price)]
    fn py_underlying_price(&self) -> f64 {
        self.underlying_price
    }

    #[getter(theoretical_price)]
    fn py_theoretical_price(&self) -> f64 {
        self.theoretical_price
    }

    #[getter(implied_volatility)]
    fn py_implied_volatility(&self) -> f64 {
        self.implied_volatility
    }

    #[getter(open_interest)]
    fn py_open_interest(&self) -> f64 {
        self.open_interest
    }

    #[getter(greeks)]
    fn py_greeks(&self) -> GreeksView {
        self.greeks
    }

    #[getter(last_price)]
    fn py_last_price(&self) -> f64 {
        self.last_price
    }

    #[getter(bid_price)]
    fn py_bid_price(&self) -> f64 {
        self.bid_price
    }

    #[getter(ask_price)]
    fn py_ask_price(&self) -> f64 {
        self.ask_price
    }

    #[getter(mid_price)]
    fn py_mid_price(&self) -> f64 {
        self.mid_price
    }

    #[getter(volume)]
    fn py_volume(&self) -> i64 {
        self.volume
    }

    #[getter(ticker)]
    fn py_ticker(&self) -> String {
        self.ticker.clone()
    }

    #[getter(intrinsic_value)]
    fn py_intrinsic_value(&self) -> f64 {
        self.intrinsic_value
    }

    #[getter(time_value)]
    fn py_time_value(&self) -> f64 {
        self.time_value
    }

    #[pyo3(name = "is_call")]
    fn py_is_call(&self) -> bool {
        self.is_call()
    }

    #[pyo3(name = "is_put")]
    fn py_is_put(&self) -> bool {
        self.is_put()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl GreeksView {
    #[getter(delta)]
    fn py_delta(&self) -> f64 {
        self.delta
    }

    #[getter(gamma)]
    fn py_gamma(&self) -> f64 {
        self.gamma
    }

    #[getter(vega)]
    fn py_vega(&self) -> f64 {
        self.vega
    }

    #[getter(theta)]
    fn py_theta(&self) -> f64 {
        self.theta
    }

    #[getter(rho)]
    fn py_rho(&self) -> f64 {
        self.rho
    }

    // `lambda` is a Python keyword; LEAN exposes the underscore alias.
    #[getter(lambda_)]
    fn py_lambda(&self) -> f64 {
        self.lambda
    }

    #[getter(theta_per_day)]
    fn py_theta_per_day(&self) -> f64 {
        self.theta / 365.0
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl UnderlyingView {
    #[getter(price)]
    fn py_price(&self) -> f64 {
        self.price
    }

    #[getter(close)]
    fn py_close(&self) -> f64 {
        self.close
    }
}

pub fn chain_underlying(chain: &OptionChain) -> UnderlyingView {
    UnderlyingView::from(chain)
}

pub fn chain_underlying_price(chain: &OptionChain) -> f64 {
    decimal_to_f64(chain.underlying_price)
}

pub fn sorted_contracts(chain: &OptionChain) -> Vec<&OptionContract> {
    chain.sorted()
}

pub fn contract_views(chain: &OptionChain) -> Vec<OptionContractView> {
    sorted_contracts(chain)
        .into_iter()
        .map(OptionContractView::from)
        .collect()
}

pub fn call_views(chain: &OptionChain) -> Vec<OptionContractView> {
    call_contracts(chain)
        .into_iter()
        .map(OptionContractView::from)
        .collect()
}

pub fn put_views(chain: &OptionChain) -> Vec<OptionContractView> {
    put_contracts(chain)
        .into_iter()
        .map(OptionContractView::from)
        .collect()
}

pub fn call_contracts(chain: &OptionChain) -> Vec<&OptionContract> {
    sorted_contracts(chain)
        .into_iter()
        .filter(|contract| contract.right == OptionRight::Call)
        .collect()
}

pub fn put_contracts(chain: &OptionChain) -> Vec<&OptionContract> {
    sorted_contracts(chain)
        .into_iter()
        .filter(|contract| contract.right == OptionRight::Put)
        .collect()
}

pub fn where_expiry_views(
    chain: &OptionChain,
    min_days: i64,
    max_days: i64,
    today: NaiveDate,
) -> Vec<OptionContractView> {
    where_expiry_contracts(chain, min_days, max_days, today)
        .into_iter()
        .map(OptionContractView::from)
        .collect()
}

pub fn where_expiry_views_from_today(
    chain: &OptionChain,
    min_days: i64,
    max_days: i64,
) -> Vec<OptionContractView> {
    where_expiry_views(chain, min_days, max_days, Local::now().date_naive())
}

pub fn where_strike_views(
    chain: &OptionChain,
    min_pct: f64,
    max_pct: f64,
) -> Vec<OptionContractView> {
    where_strike_contracts(chain, min_pct, max_pct)
        .into_iter()
        .map(OptionContractView::from)
        .collect()
}

pub fn where_expiry_contracts(
    chain: &OptionChain,
    min_days: i64,
    max_days: i64,
    today: NaiveDate,
) -> Vec<&OptionContract> {
    sorted_contracts(chain)
        .into_iter()
        .filter(|contract| {
            let days = (contract.expiry - today).num_days();
            days >= min_days && days <= max_days
        })
        .collect()
}

pub fn where_expiry_contracts_from_today(
    chain: &OptionChain,
    min_days: i64,
    max_days: i64,
) -> Vec<&OptionContract> {
    where_expiry_contracts(chain, min_days, max_days, Local::now().date_naive())
}

pub fn where_strike_contracts(
    chain: &OptionChain,
    min_pct: f64,
    max_pct: f64,
) -> Vec<&OptionContract> {
    let spot = chain.underlying_price;
    if spot.is_zero() {
        return vec![];
    }

    let lo = spot * Decimal::from_f64(1.0 + min_pct).unwrap_or(Decimal::ONE);
    let hi = spot * Decimal::from_f64(1.0 + max_pct).unwrap_or(Decimal::ONE);

    sorted_contracts(chain)
        .into_iter()
        .filter(|contract| contract.strike >= lo && contract.strike <= hi)
        .collect()
}

pub fn chain_lookup_key_from_str(key: &str) -> String {
    key.to_string()
}

pub fn chain_lookup_key_from_symbol(symbol: &Symbol) -> String {
    symbol.permtick.to_string()
}

pub fn calculate_implied_volatility(
    contract: &OptionContract,
    valuation_time: DateTime,
    option_price: f64,
    underlying_price: Option<f64>,
    risk_free_rate: f64,
    dividend_yield: f64,
) -> Option<f64> {
    let spot =
        underlying_price.unwrap_or_else(|| decimal_to_f64(contract.data.underlying_last_price));
    let strike = decimal_to_f64(contract.strike);
    let t = time_to_expiry_years(contract.expiry, valuation_time);
    if option_price <= 0.0 || spot <= 0.0 || strike <= 0.0 || t <= 0.0 {
        return None;
    }
    let iv = implied_volatility(
        option_price,
        spot,
        strike,
        t,
        risk_free_rate,
        dividend_yield,
        contract.right,
    );
    if iv.is_finite() && iv > 0.0 {
        Some(iv)
    } else {
        None
    }
}

pub fn decimal_to_f64(value: Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, SymbolOptionsExt};
    use lean_options::{OptionChain, OptionContract, OptionContractData};
    use rust_decimal_macros::dec;

    fn equity_symbol(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    fn option_contract(
        underlying: &Symbol,
        strike: Decimal,
        expiry: NaiveDate,
        right: OptionRight,
    ) -> OptionContract {
        let symbol = Symbol::create_option_osi(
            underlying.clone(),
            strike,
            expiry,
            right,
            OptionStyle::American,
            &Market::usa(),
        );
        let mut contract = OptionContract::new(symbol);
        contract.data = OptionContractData {
            theoretical_price: dec!(1.23),
            implied_volatility: dec!(0.42),
            greeks: Greeks {
                delta: dec!(-0.25),
                gamma: dec!(0.02),
                vega: dec!(0.11),
                theta: dec!(-7.3),
                rho: dec!(0.04),
                lambda: dec!(-3.5),
            },
            open_interest: dec!(1234),
            last_price: dec!(2.10),
            volume: 77,
            bid_price: dec!(2.00),
            bid_size: 10,
            ask_price: dec!(2.20),
            ask_size: 11,
            underlying_last_price: dec!(411),
        };
        contract
    }

    fn sample_chain() -> OptionChain {
        let underlying = equity_symbol("SPY");
        let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
        let mut chain = OptionChain::new(canonical, dec!(411));
        chain.add_contract(option_contract(
            &underlying,
            dec!(412),
            NaiveDate::from_ymd_opt(2024, 2, 16).unwrap(),
            OptionRight::Put,
        ));
        chain.add_contract(option_contract(
            &underlying,
            dec!(410),
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            OptionRight::Put,
        ));
        chain.add_contract(option_contract(
            &underlying,
            dec!(410),
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            OptionRight::Call,
        ));
        chain
    }

    #[test]
    fn contract_view_projects_contract_values() {
        let underlying = equity_symbol("SPY");
        let contract = option_contract(
            &underlying,
            dec!(410),
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            OptionRight::Put,
        );

        let view = OptionContractView::from(&contract);

        assert_eq!(view.strike, 410.0);
        assert_eq!(
            view.expiry,
            NaiveDate::from_ymd_opt(2024, 1, 19)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        );
        assert_eq!(view.right, OptionRight::Put);
        assert_eq!(view.style.as_str(), "American");
        assert_eq!(view.underlying_price, 411.0);
        assert_eq!(view.theoretical_price, 1.23);
        assert_eq!(view.implied_volatility, 0.42);
        assert_eq!(view.open_interest, 1234.0);
        assert_eq!(view.greeks.delta, -0.25);
        assert_eq!(view.greeks.theta_per_day(), -0.02);
        assert_eq!(view.last_price, 2.10);
        assert_eq!(view.bid_price, 2.00);
        assert_eq!(view.ask_price, 2.20);
        assert_eq!(view.mid_price, 2.10);
        assert_eq!(view.volume, 77);
        assert_eq!(view.intrinsic_value, 0.0);
        assert_eq!(view.time_value, 2.10);
        assert!(view.is_put());
        assert!(!view.is_call());
    }

    #[test]
    fn chain_views_are_deterministic_and_filterable() {
        let chain = sample_chain();
        let view = OptionChainView::from_chain(&chain);

        let all = contract_views(&chain);
        assert_eq!(all.len(), 3);
        assert_eq!(view.__len__(), 3);
        assert_eq!(view.__iter__().len(), 3);
        assert_eq!(
            all[0].expiry.date(),
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap()
        );
        assert_eq!(all[0].strike, 410.0);
        assert_eq!(all[0].right, OptionRight::Call);
        assert_eq!(all[1].right, OptionRight::Put);

        let calls = call_views(&chain);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].right, OptionRight::Call);

        let puts = put_views(&chain);
        assert_eq!(puts.len(), 2);
        assert!(puts.iter().all(OptionContractView::is_put));
    }

    #[test]
    fn chain_views_project_underlying_and_lookup_keys() {
        let chain = sample_chain();

        assert_eq!(chain_underlying_price(&chain), 411.0);
        assert_eq!(
            chain_underlying(&chain),
            UnderlyingView {
                price: 411.0,
                close: 411.0
            }
        );
        assert_eq!(chain_lookup_key_from_str("?SPY"), "?SPY");
        assert_eq!(
            chain_lookup_key_from_symbol(&chain.canonical_symbol),
            "?SPY"
        );
    }

    #[test]
    fn chain_where_filters_match_lean_helpers() {
        let chain = sample_chain();
        let today = NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();

        let near_expiry = where_expiry_views(&chain, 1, 30, today);
        assert_eq!(near_expiry.len(), 2);
        assert!(near_expiry
            .iter()
            .all(|view| view.expiry.date() == NaiveDate::from_ymd_opt(2024, 1, 19).unwrap()));

        let near_spot = where_strike_views(&chain, -0.005, 0.0);
        assert_eq!(near_spot.len(), 2);
        assert!(near_spot.iter().all(|view| view.strike == 410.0));
    }

    #[test]
    fn option_filter_universe_returns_empty_when_underlying_price_is_zero() {
        // Mirrors LEAN OptionFilterTests boundary behavior: strike-relative
        // filters cannot select contracts without a valid underlying price.
        let underlying = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let canonical = Symbol::create_option(
            underlying.clone(),
            &lean_core::Market::usa(),
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            dec!(410),
            OptionRight::Call,
            lean_core::OptionStyle::American,
        );
        let mut chain = OptionChain::new(canonical, dec!(0));
        chain.add_contract(option_contract(
            &underlying,
            dec!(410),
            NaiveDate::from_ymd_opt(2024, 1, 19).unwrap(),
            OptionRight::Call,
        ));

        assert!(where_strike_contracts(&chain, -0.05, 0.05).is_empty());
        assert!(where_strike_views(&chain, -0.05, 0.05).is_empty());
    }
}
