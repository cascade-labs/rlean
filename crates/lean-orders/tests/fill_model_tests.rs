use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::TradeBar;
use lean_orders::{
    fill_model::{FillModel, ImmediateFillModel},
    slippage::ConstantSlippageModel,
    Order,
};
use rust_decimal_macros::dec;

fn ts(i: i64) -> NanosecondTimestamp {
    NanosecondTimestamp::from_secs(i * 86400)
}
fn spy() -> Symbol {
    Symbol::create_equity("SPY", &Market::usa())
}

fn make_bar(open: f64, high: f64, low: f64, close: f64) -> TradeBar {
    use rust_decimal::Decimal;
    use std::str::FromStr;
    TradeBar {
        symbol: spy(),
        time: ts(0),
        end_time: ts(1),
        open: Decimal::from_str(&open.to_string()).unwrap(),
        high: Decimal::from_str(&high.to_string()).unwrap(),
        low: Decimal::from_str(&low.to_string()).unwrap(),
        close: Decimal::from_str(&close.to_string()).unwrap(),
        volume: dec!(100000),
        period: TimeSpan::ONE_DAY,
    }
}

fn no_slippage_model() -> ImmediateFillModel {
    ImmediateFillModel::new(Box::new(ConstantSlippageModel::new(dec!(0))))
}

// ─── Market fill ─────────────────────────────────────────────────────────────

#[test]
fn market_fill_at_open() {
    let model = no_slippage_model();
    let bar = make_bar(100.0, 105.0, 95.0, 102.0);
    let order = Order::market(1, spy(), dec!(100), ts(0), "");

    let fill = model.market_fill(&order, &bar, ts(0));
    assert_eq!(fill.order_event.fill_price, dec!(100)); // open price
    assert_eq!(fill.order_event.fill_quantity, dec!(100));
}

// ─── Limit fill ──────────────────────────────────────────────────────────────

#[test]
fn buy_limit_fills_when_low_touches_limit() {
    let model = no_slippage_model();
    // Bar low=95, limit=97 → 95 <= 97, should fill
    let bar = make_bar(100.0, 105.0, 95.0, 102.0);
    let order = Order::limit(1, spy(), dec!(100), dec!(97), ts(0), "");

    let fill = model.limit_fill(&order, &bar, ts(0));
    assert!(fill.is_some(), "Limit order should fill when low <= limit");
    // Fill price is min(limit, open) = min(97, 100) = 97
    assert_eq!(fill.unwrap().order_event.fill_price, dec!(97));
}

#[test]
fn buy_limit_does_not_fill_when_low_above_limit() {
    let model = no_slippage_model();
    // Bar low=99, limit=97 → 99 > 97, should NOT fill
    let bar = make_bar(100.0, 105.0, 99.0, 102.0);
    let order = Order::limit(1, spy(), dec!(100), dec!(97), ts(0), "");

    let fill = model.limit_fill(&order, &bar, ts(0));
    assert!(
        fill.is_none(),
        "Limit order should not fill when low > limit"
    );
}

#[test]
fn sell_limit_fills_when_high_touches_limit() {
    let model = no_slippage_model();
    // Sell limit at 105, bar high=107 → 107 >= 105, should fill
    let bar = make_bar(100.0, 107.0, 95.0, 102.0);
    let order = Order::limit(1, spy(), dec!(-100), dec!(105), ts(0), "");

    let fill = model.limit_fill(&order, &bar, ts(0));
    assert!(fill.is_some(), "Sell limit should fill when high >= limit");
}

#[test]
fn sell_limit_does_not_fill_when_high_below_limit() {
    let model = no_slippage_model();
    // Sell limit at 110, bar high=107 → 107 < 110, should NOT fill
    let bar = make_bar(100.0, 107.0, 95.0, 102.0);
    let order = Order::limit(1, spy(), dec!(-100), dec!(110), ts(0), "");

    let fill = model.limit_fill(&order, &bar, ts(0));
    assert!(
        fill.is_none(),
        "Sell limit should not fill when high < limit"
    );
}

// ─── Stop market fill ────────────────────────────────────────────────────────

#[test]
fn buy_stop_fills_when_high_reaches_stop() {
    let model = no_slippage_model();
    // Buy stop at 103, bar high=107 → 107 >= 103, should fill
    let bar = make_bar(100.0, 107.0, 95.0, 102.0);
    let order = Order::stop_market(1, spy(), dec!(100), dec!(103), ts(0), "");

    let fill = model.stop_market_fill(&order, &bar, ts(0));
    assert!(fill.is_some(), "Buy stop should fill when high >= stop");
}

#[test]
fn buy_stop_does_not_fill_when_high_below_stop() {
    let model = no_slippage_model();
    // Buy stop at 110, bar high=107 → 107 < 110, should NOT fill
    let bar = make_bar(100.0, 107.0, 95.0, 102.0);
    let order = Order::stop_market(1, spy(), dec!(100), dec!(110), ts(0), "");

    let fill = model.stop_market_fill(&order, &bar, ts(0));
    assert!(fill.is_none(), "Buy stop should not fill when high < stop");
}

// ─── Market on close ────────────────────────────────────────────────────────

#[test]
fn market_on_close_fills_at_close() {
    let model = no_slippage_model();
    let bar = make_bar(100.0, 105.0, 95.0, 102.0);
    let order = Order::market(1, spy(), dec!(100), ts(0), "");

    let fill = model.market_on_close_fill(&order, &bar, ts(0));
    assert_eq!(fill.order_event.fill_price, dec!(102)); // close price
}
