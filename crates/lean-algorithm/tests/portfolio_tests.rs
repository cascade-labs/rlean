use lean_algorithm::portfolio::{SecurityHolding, SecurityPortfolioManager};
use lean_core::{Market, NanosecondTimestamp, Symbol};
use lean_orders::{Order, OrderStatus};
use rust_decimal_macros::dec;

// ─── Option exercise/assignment equity accounting ────────────────────────────
//
// These tests validate the core invariant: when an option is settled
// (exercise or assignment), the portfolio total value must be unchanged
// (minus any market-price slippage vs strike, which is zero at strike).
//
// Before fix: process_option_expirations() manually debited cash AND queued a
// market order, causing a phantom equity drop for one full trading day because
// the cash was gone but the stock holding did not yet exist.
//
// After fix: apply_exercise() atomically creates the holding AND adjusts cash
// so the equity is always consistent.

fn ts() -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(0)
}
fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}
fn aapl() -> Symbol {
    Symbol::create_equity("AAPL", &Market::usa())
}

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

// ─── apply_exercise: short put assignment ─────────────────────────────────────

/// Scenario: we sold a put at K=$456 for $10/share premium ($1,000 total).
/// Premium receipt: cash += $1,000.  Starting cash = $60,000.
/// On assignment: buy 100 shares at K=$456.
///
/// Expected equity after settlement:
///   cash    = $60,000 + $1,000 - $45,600  = $15,400
///   holding = 100 × $456                  = $45,600
///   total   = $61,000  (starting $60K + $1K premium — no phantom drawdown)
#[test]
fn short_put_assignment_preserves_equity() {
    let pm = SecurityPortfolioManager::new(dec!(60_000));

    // Credit the option premium when the put was sold.
    *pm.cash.write() += dec!(1_000);
    assert_eq!(*pm.cash.read(), dec!(61_000));

    // Assignment: buy 100 SPY shares at strike $456.
    pm.apply_exercise(&spy(), dec!(456), dec!(100));

    let cash = *pm.cash.read();
    let total = pm.total_portfolio_value();

    // Cash must be reduced by exactly strike × shares.
    assert_eq!(cash, dec!(15_400), "cash after put assignment");
    // Holdings must be worth strike × shares immediately.
    assert_eq!(
        pm.total_holdings_value(),
        dec!(45_600),
        "holdings after put assignment"
    );
    // Total portfolio value must equal starting + premium (no phantom drawdown).
    assert_eq!(total, dec!(61_000), "total equity after put assignment");
}

/// Equity is stable next-day even after market price moves away from strike.
#[test]
fn short_put_assignment_equity_after_price_update() {
    let pm = SecurityPortfolioManager::new(dec!(60_000));
    *pm.cash.write() += dec!(1_000); // premium received

    pm.apply_exercise(&spy(), dec!(456), dec!(100)); // assignment at K=456

    // Next-day SPY moves to $470.
    pm.update_prices(&spy(), dec!(470));

    let total = pm.total_portfolio_value();
    // cash = 15,400; holding = 100 × 470 = 47,000 → total = 62,400
    assert_eq!(total, dec!(62_400));
}

// ─── apply_exercise: short call assignment ────────────────────────────────────

/// Short call assigned: we must SELL 100 shares at K=$435.
/// We already own 100 shares (from prior put assignment or direct purchase).
/// Selling delivers cash of strike × shares.
#[test]
fn short_call_assignment_preserves_equity() {
    let pm = SecurityPortfolioManager::new(dec!(60_000));

    // First own 100 SPY shares (bought at $437 via prior put assignment).
    pm.apply_exercise(&spy(), dec!(437), dec!(100));

    // Credit call premium.
    *pm.cash.write() += dec!(1_100);

    // Call assigned: sell 100 shares at K=$435 (below cost — small loss on shares, offset by premium).
    pm.apply_exercise(&spy(), dec!(435), dec!(-100));

    let cash_after = *pm.cash.read();
    let holdings_after = pm.total_holdings_value();
    let equity_after = pm.total_portfolio_value();

    // After selling all shares, no holdings remain.
    assert_eq!(holdings_after, dec!(0), "no holdings after call assignment");
    // Cash = starting - 43,700 (put) + 1,100 (call premium) + 43,500 (call assignment)
    //      = 60,000 - 43,700 + 1,100 + 43,500 = 60,900
    assert_eq!(cash_after, dec!(60_900), "cash after call assignment");
    assert_eq!(equity_after, dec!(60_900), "total after call assignment");
    // Equity changes by net P&L of the trade: (435-437)*100 + 1100 = -200 + 1100 = 900
    let net_pnl = equity_after - dec!(60_000); // vs starting cash
    assert_eq!(net_pnl, dec!(900), "net P&L = premium - loss on strike");
}

// ─── apply_exercise: long call exercise ──────────────────────────────────────

/// Long call exercised: buy 100 shares at K=$450, pay $45,000 from cash.
#[test]
fn long_call_exercise_preserves_equity() {
    let pm = SecurityPortfolioManager::new(dec!(60_000));
    // Paid $500 premium for the call.
    *pm.cash.write() -= dec!(500);

    // Exercise: buy 100 shares at K=$450.
    pm.apply_exercise(&spy(), dec!(450), dec!(100));

    // cash = 60,000 - 500 - 45,000 = 14,500
    assert_eq!(*pm.cash.read(), dec!(14_500));
    // holding = 100 × 450 = 45,000
    assert_eq!(pm.total_holdings_value(), dec!(45_000));
    // total = 59,500 (= 60,000 - 500 premium paid at open)
    assert_eq!(pm.total_portfolio_value(), dec!(59_500));
}

// ─── apply_exercise: long put exercise ───────────────────────────────────────

/// Long put exercised: sell 100 shares at K=$430, receive $43,000.
/// We own the shares (bought at $450 earlier).
#[test]
fn long_put_exercise_preserves_equity() {
    let pm = SecurityPortfolioManager::new(dec!(60_000));

    // Buy 100 shares at $450.
    pm.apply_exercise(&spy(), dec!(450), dec!(100));
    // Pay $300 premium for a protective put at K=$430.
    *pm.cash.write() -= dec!(300);

    // Spot drops to $400. Exercise put: sell 100 shares at K=$430.
    pm.apply_exercise(&spy(), dec!(430), dec!(-100));

    // cash = 60,000 - 45,000 - 300 + 43,000 = 57,700
    assert_eq!(*pm.cash.read(), dec!(57_700));
    assert_eq!(pm.total_holdings_value(), dec!(0));
    assert_eq!(pm.total_portfolio_value(), dec!(57_700));

    // Net P&L: -$2,000 from stock (bought 450, sold 430) - $300 premium = -$2,300
    let net_pnl = pm.total_portfolio_value() - dec!(60_000);
    assert_eq!(net_pnl, dec!(-2_300));
}

// ─── Regression: old bug would show phantom drawdown ─────────────────────────

/// Reproduces the pre-fix behaviour to confirm it was broken.
/// (For documentation: this test would FAIL with the old code path where
///  cash was manually debited but no holding was created atomically.)
#[test]
fn no_phantom_drawdown_on_assignment_day() {
    let pm = SecurityPortfolioManager::new(dec!(60_000));
    *pm.cash.write() += dec!(1_000); // put premium received

    // With apply_exercise, the holding is created ATOMICALLY with the cash debit.
    // Total value must not drop below $61,000 at any point.
    pm.apply_exercise(&spy(), dec!(456), dec!(100));

    // Immediately after settlement (same "day"), equity is still correct.
    let total = pm.total_portfolio_value();
    assert!(
        total >= dec!(60_000),
        "equity must never drop below starting cash on assignment day; got {total}"
    );
    assert_eq!(total, dec!(61_000));
}
