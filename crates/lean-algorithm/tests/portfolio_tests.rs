use lean_algorithm::portfolio::{SecurityHolding, SecurityPortfolioManager};
use lean_core::{Market, NanosecondTimestamp, Symbol};
use lean_orders::{Order, OrderStatus};
use rust_decimal_macros::dec;

fn ts() -> NanosecondTimestamp { NanosecondTimestamp::from_secs(0) }
fn spy() -> Symbol { Symbol::create_equity("SPY", &Market::usa()) }
fn aapl() -> Symbol { Symbol::create_equity("AAPL", &Market::usa()) }

// ─── SecurityHolding ─────────────────────────────────────────────────────────

#[test]
fn new_holding_is_flat() {
    let h = SecurityHolding::new(spy());
    assert!(!h.is_invested());
    assert!(!h.is_long());
    assert!(!h.is_short());
    assert_eq!(h.quantity, dec!(0));
    assert_eq!(h.market_value(), dec!(0));
}

/// Opening a new long position sets average price to fill price
#[test]
fn apply_fill_opens_long_position() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    assert!(h.is_long());
    assert_eq!(h.quantity, dec!(50));
    assert_eq!(h.average_price, dec!(100));
}

/// Opening a new short position
#[test]
fn apply_fill_opens_short_position() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(-50), dec!(0));
    assert!(h.is_short());
    assert_eq!(h.quantity, dec!(-50));
    assert_eq!(h.average_price, dec!(100));
}

/// Adding to a long position updates VWAP
/// VWAP(100*50 + 110*50) / 100 = (5000+5500)/100 = 105
#[test]
fn adding_to_long_updates_vwap() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    h.apply_fill(dec!(110), dec!(50), dec!(0));
    assert_eq!(h.quantity, dec!(100));
    assert_eq!(h.average_price, dec!(105));
}

/// Partial reduction: close 20 of 50 shares, check realized PnL
/// Bought at 100, selling 20 at 120 → PnL = (120-100)*20 = 400
#[test]
fn partial_reduction_realizes_pnl() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    h.apply_fill(dec!(120), dec!(-20), dec!(0));

    assert_eq!(h.quantity, dec!(30));
    assert_eq!(h.realized_pnl, dec!(400));
    // Average price unchanged for partial reduction
    assert_eq!(h.average_price, dec!(100));
}

/// Full close: realized PnL correct, average_price reset to 0
#[test]
fn full_close_resets_average_price() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    h.apply_fill(dec!(120), dec!(-50), dec!(0));

    assert_eq!(h.quantity, dec!(0));
    assert!(!h.is_invested());
    assert_eq!(h.realized_pnl, dec!(1000)); // (120-100)*50
    assert_eq!(h.average_price, dec!(0));
}

/// Position reversal: long 50 → short 30 after selling 80
/// Realized PnL on 50 shares at profit, new short at fill price
#[test]
fn position_reversal() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    // Sell 80 = close 50 (PnL = (110-100)*50 = 500) + open short 30
    h.apply_fill(dec!(110), dec!(-80), dec!(0));

    assert_eq!(h.quantity, dec!(-30));
    assert_eq!(h.realized_pnl, dec!(500));
    assert_eq!(h.average_price, dec!(110)); // new short at reversal price
}

/// Fees accumulate
#[test]
fn fees_accumulate() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(100), dec!(1));
    h.apply_fill(dec!(110), dec!(-100), dec!(1));
    assert_eq!(h.total_fees, dec!(2));
}

/// Market value = quantity × last_price
#[test]
fn market_value() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    h.update_price(dec!(120));
    assert_eq!(h.market_value(), dec!(6000));
}

/// Unrealized PnL = (price - avg) × quantity
#[test]
fn unrealized_pnl() {
    let mut h = SecurityHolding::new(spy());
    h.apply_fill(dec!(100), dec!(50), dec!(0));
    h.update_price(dec!(110));
    assert_eq!(h.unrealized_pnl, dec!(500)); // (110-100)*50
}

// ─── SecurityPortfolioManager ─────────────────────────────────────────────────

#[test]
fn portfolio_initial_state() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    assert_eq!(*pm.cash.read(), dec!(100_000));
    assert_eq!(pm.total_portfolio_value(), dec!(100_000));
}

#[test]
fn portfolio_apply_fill_updates_cash() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    let mut order = Order::market(1, spy(), dec!(100), ts(), "");
    order.status = OrderStatus::Filled;

    pm.apply_fill(&order, dec!(100), dec!(100), dec!(0));
    // Bought 100 shares at $100 → cash reduced by $10,000
    assert_eq!(*pm.cash.read(), dec!(90_000));
}

#[test]
fn portfolio_total_value_includes_holdings() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    let order = Order::market(1, spy(), dec!(100), ts(), "");

    pm.apply_fill(&order, dec!(100), dec!(100), dec!(0));
    // Cash = 90,000, Holdings = 100 shares × $100 = $10,000 → total = $100,000
    pm.update_prices(&spy(), dec!(100));
    assert_eq!(pm.total_portfolio_value(), dec!(100_000));
}

#[test]
fn portfolio_total_return_pct() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    let order = Order::market(1, spy(), dec!(100), ts(), "");
    pm.apply_fill(&order, dec!(100), dec!(100), dec!(0));
    // Price rises to 110
    pm.update_prices(&spy(), dec!(110));
    // Holdings = 100 × 110 = 11,000, cash = 90,000, total = 101,000
    // return = (101,000 - 100,000) / 100,000 = 1%
    let ret = pm.total_return_pct();
    assert_eq!(ret, dec!(0.01));
}

#[test]
fn portfolio_is_invested_check() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    assert!(!pm.is_invested(&spy()));

    let order = Order::market(1, spy(), dec!(100), ts(), "");
    pm.apply_fill(&order, dec!(100), dec!(100), dec!(0));
    assert!(pm.is_invested(&spy()));
    assert!(!pm.is_invested(&aapl()));
}

#[test]
fn portfolio_unrealized_profit() {
    let pm = SecurityPortfolioManager::new(dec!(100_000));
    let order = Order::market(1, spy(), dec!(100), ts(), "");
    pm.apply_fill(&order, dec!(100), dec!(100), dec!(0));
    pm.update_prices(&spy(), dec!(150));
    // 100 shares × (150-100) = 5,000
    assert_eq!(pm.unrealized_profit(), dec!(5000));
}
