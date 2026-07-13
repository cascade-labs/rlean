use rlean_algorithm::lifecycle::{
    AlgorithmHistoryService, AlgorithmRuntimeServices, HistoryColumns, NullHistoryService,
    RegisteredIndicatorRegistry,
};
use rlean_algorithm::qc_algorithm::QcAlgorithm;
use rlean_core::{Resolution, SecurityType};
use rlean_orders::order::TimeInForce;
use rlean_sdk::algorithm::{
    AlgorithmConstructionContext, AlgorithmHandle, BrokerageModelSecurityInitializerHandle,
    FuncSecuritySeederHandle, SecuritySeedFn,
};
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

struct TestRuntimeServices;

impl AlgorithmHistoryService for TestRuntimeServices {
    fn history(
        &self,
        _algorithm: &QcAlgorithm,
        _symbol: &rlean_core::Symbol,
        _periods: usize,
        _resolution: Resolution,
    ) -> HistoryColumns {
        HistoryColumns::new()
    }
}

impl AlgorithmRuntimeServices for TestRuntimeServices {
    fn history_service(&self) -> Arc<dyn AlgorithmHistoryService> {
        Arc::new(NullHistoryService)
    }

    fn runtime_parameters(&self) -> Arc<RwLock<HashMap<String, String>>> {
        Arc::new(RwLock::new(HashMap::new()))
    }

    fn registered_indicators(&self) -> RegisteredIndicatorRegistry {
        Arc::new(Mutex::new(HashMap::new()))
    }
}

fn test_algorithm() -> AlgorithmHandle {
    let context = AlgorithmConstructionContext::new_with_runtime_services(
        Arc::new(Mutex::new(QcAlgorithm::new("Algorithm", dec!(100000)))),
        Arc::new(TestRuntimeServices),
    );
    AlgorithmHandle::with_default_context(context, AlgorithmHandle::default_algorithm)
}

#[test]
fn algorithm_handle_projects_cash_and_portfolio_like_qc_algorithm() {
    let algorithm = test_algorithm();

    assert_close(algorithm.cash(), 100_000.0);
    assert_close(algorithm.portfolio().cash_f64(), 100_000.0);
    assert_close(algorithm.portfolio_value(), 100_000.0);

    algorithm.set_cash(12_345.67);
    assert_close(algorithm.cash(), 12_345.67);
    assert_close(algorithm.portfolio().total_portfolio_value(), 12_345.67);

    algorithm.add_cash(54.33);
    assert_close(algorithm.cash(), 12_400.0);
    assert_close(algorithm.portfolio().cash_f64(), 12_400.0);
}

#[test]
fn algorithm_handle_adds_and_removes_common_security_types() {
    let algorithm = test_algorithm();

    let equity = algorithm.add_equity("spy".to_string(), Resolution::Daily, None);
    let equity_symbol = equity.symbol();
    assert_eq!(equity_symbol.value(), "SPY");
    assert_eq!(equity_symbol.inner().security_type(), SecurityType::Equity);
    assert!(algorithm.has_security(equity_symbol.clone()));

    let forex_symbol = algorithm.add_forex("eurusd".to_string(), Resolution::Hour);
    assert_eq!(forex_symbol.value(), "EURUSD");
    assert_eq!(forex_symbol.inner().security_type(), SecurityType::Forex);
    assert!(algorithm.has_security(forex_symbol));

    let crypto_symbol = algorithm.add_crypto(
        "btcusd".to_string(),
        Some("coinbase".to_string()),
        Resolution::Minute,
    );
    assert_eq!(crypto_symbol.value(), "BTCUSD");
    assert_eq!(crypto_symbol.inner().security_type(), SecurityType::Crypto);
    assert!(algorithm.has_security(crypto_symbol));

    assert!(algorithm.remove_security(equity_symbol.clone(), Some("test removal".to_string())));
    assert!(!algorithm.has_security(equity_symbol));
}

#[test]
fn algorithm_handle_order_helpers_return_lean_ticket_projections() {
    let algorithm = test_algorithm();
    let security = algorithm.add_equity("spy".to_string(), Resolution::Minute, None);
    let symbol = security.symbol();

    let market = algorithm.market_order(symbol.clone(), 10.0, Some(TimeInForce::Day), Some(false));
    assert_eq!(market.symbol(), Some(symbol.inner().clone()));
    assert_close(market.quantity(), 10.0);
    assert!(market.order_id() > 0);

    let limit = algorithm.limit_order(symbol.clone(), -5.0, 401.25, None, true, false);
    assert_eq!(limit.symbol(), Some(symbol.inner().clone()));
    assert_close(limit.quantity(), -5.0);
    assert_eq!(limit.limit_price(), Some(401.25));

    let stop = algorithm.stop_market_order(symbol, 3.0, 399.5, None, false);
    assert_close(stop.quantity(), 3.0);
    assert_eq!(stop.stop_price(), Some(399.5));
}

#[test]
fn security_initializer_seeder_seeds_price_on_add() {
    let algorithm = test_algorithm();

    // Native seed function mirroring LEAN's FuncSecuritySeeder seed function.
    let seed_fn: SecuritySeedFn = Arc::new(|_security| Some(411.25));
    let seeder = FuncSecuritySeederHandle::from_fn(seed_fn);
    algorithm
        .set_security_initializer(BrokerageModelSecurityInitializerHandle::with_seeder(seeder));

    // The test history service returns no last-known price, so absent the seeder
    // the price would stay zero. The seeder must set it on add.
    let security = algorithm.add_equity("spy".to_string(), Resolution::Daily, None);
    let symbol = security.symbol();
    let price = algorithm.securities().get(symbol.inner()).price();
    assert_close(price, 411.25);
}

#[test]
fn security_initializer_seeder_skips_canonical_option() {
    let algorithm = test_algorithm();

    let seed_fn: SecuritySeedFn = Arc::new(|_security| Some(999.0));
    algorithm.set_security_initializer(BrokerageModelSecurityInitializerHandle::with_seeder(
        FuncSecuritySeederHandle::from_fn(seed_fn),
    ));

    // add_option registers a canonical option symbol; LEAN's FuncSecuritySeeder
    // skips canonical symbols, so no seed price is applied to the canonical.
    let option = algorithm.add_option("spy".to_string(), Resolution::Daily);
    let canonical = option.symbol();
    let price = algorithm.securities().get(canonical.inner()).price();
    assert_close(price, 0.0);
}
