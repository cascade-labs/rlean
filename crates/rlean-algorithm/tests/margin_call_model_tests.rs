use rlean_algorithm::margin_call::{
    build_margin_call_context, DefaultMarginCallModel, MarginCallModel, MarginCallModelKind,
};
use rlean_algorithm::qc_algorithm::QcAlgorithm;
use rlean_algorithm::securities::Security;
use rlean_core::exchange_hours::ExchangeHours;
use rlean_core::{Market, Resolution, Symbol, SymbolProperties};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;

fn aapl_security(holdings: rlean_algorithm::portfolio::SharedHoldings) -> Security {
    let symbol = Symbol::create_equity("AAPL", &Market::usa());
    Security::new(
        symbol,
        Resolution::Daily,
        SymbolProperties::default(),
        Arc::new(ExchangeHours::us_equity()),
        holdings,
    )
}

fn setup_leveraged_portfolio(
    cash: Decimal,
    quantity: Decimal,
    price: Decimal,
    leverage: f64,
) -> QcAlgorithm {
    let mut algorithm = QcAlgorithm::new("margin-call-test", cash);
    let security = aapl_security(algorithm.portfolio.holdings_store());
    security.set_leverage(leverage);
    security.set_price(price);
    algorithm.securities.add(security);
    let symbol = Symbol::create_equity("AAPL", &Market::usa());
    algorithm
        .portfolio
        .set_holdings(&symbol, price, quantity, dec!(1));
    algorithm.portfolio.update_prices(&symbol, price);
    algorithm
}

#[test]
fn margin_remaining_matches_total_portfolio_minus_margin_used() {
    let algorithm = setup_leveraged_portfolio(dec!(1000), dec!(1000), dec!(1), 2.0);
    let portfolio = algorithm.portfolio.clone();
    let used = algorithm.total_margin_used();
    let remaining = portfolio.margin_remaining_with_used(used);
    assert_eq!(remaining, portfolio.total_portfolio_value() - used);
}

#[test]
fn unleveraged_position_at_full_margin_sets_warning_without_orders() {
    let algorithm = setup_leveraged_portfolio(dec!(0), dec!(1000), dec!(0.50), 1.0);
    let portfolio = algorithm.portfolio.clone();
    let ctx = build_margin_call_context(&portfolio, &algorithm);
    let model = DefaultMarginCallModel::new();
    let (orders, warning) = model.get_margin_call_orders(&ctx);
    assert!(warning);
    assert!(orders.is_empty());
}

#[test]
fn leveraged_underwater_portfolio_issues_margin_call_orders() {
    let algorithm = setup_leveraged_portfolio(dec!(-250), dec!(1000), dec!(0.40), 2.0);
    let portfolio = algorithm.portfolio.clone();
    let ctx = build_margin_call_context(&portfolio, &algorithm);
    let model = DefaultMarginCallModel::with_margin_buffer(Decimal::ZERO);
    let (orders, warning) = model.get_margin_call_orders(&ctx);
    assert!(warning);
    assert_eq!(orders.len(), 1);
    assert_ne!(orders[0].quantity, Decimal::ZERO);
}

#[test]
fn margin_call_warning_at_five_percent_remaining() {
    let algorithm = setup_leveraged_portfolio(dec!(-475), dec!(1000), dec!(1), 2.0);
    let portfolio = algorithm.portfolio.clone();
    let ctx = build_margin_call_context(&portfolio, &algorithm);
    let remaining = ctx.margin_remaining();
    assert!(remaining > Decimal::ZERO);
    assert!(remaining <= ctx.total_portfolio_value * dec!(0.05));

    let model = DefaultMarginCallModel::new();
    let (orders, warning) = model.get_margin_call_orders(&ctx);
    assert!(warning);
    assert!(orders.is_empty());
}

#[test]
fn null_margin_call_model_is_disabled_for_live() {
    let model = MarginCallModelKind::live_disabled();
    assert!(model.is_null());
    let algorithm = setup_leveraged_portfolio(dec!(-250), dec!(1000), dec!(0.50), 2.0);
    let portfolio = algorithm.portfolio.clone();
    let ctx = build_margin_call_context(&portfolio, &algorithm);
    let (orders, warning) = model.get_margin_call_orders(&ctx);
    assert!(!warning);
    assert!(orders.is_empty());
}

#[test]
fn check_backtest_bankruptcy_stops_at_zero_equity() {
    use rlean_algorithm::margin_call::check_backtest_bankruptcy;
    assert!(check_backtest_bankruptcy(true, dec!(0)));
    assert!(check_backtest_bankruptcy(true, dec!(-1)));
    assert!(!check_backtest_bankruptcy(true, dec!(1)));
    assert!(!check_backtest_bankruptcy(false, dec!(-1)));
}

#[test]
fn maximum_order_quantity_for_delta_reduces_long_position() {
    use rlean_algorithm::buying_power::BuyingPowerModel;
    let algorithm = setup_leveraged_portfolio(dec!(1000), dec!(1000), dec!(1), 2.0);
    let holding = algorithm
        .portfolio
        .get_holding(&Symbol::create_equity("AAPL", &Market::usa()));
    let result = BuyingPowerModel::maximum_order_quantity_for_delta_buying_power(
        BuyingPowerModel::SecurityMargin,
        &holding,
        2.0,
        dec!(1),
        algorithm.portfolio.total_portfolio_value(),
        dec!(-100),
        Decimal::ZERO,
        algorithm.margin_remaining(),
        |_| Decimal::ZERO,
    );
    assert!(result.quantity < Decimal::ZERO);
}
