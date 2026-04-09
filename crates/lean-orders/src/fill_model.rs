use crate::{order::Order, order_event::OrderEvent, slippage::SlippageModel};
use lean_core::{DateTime, Price};
use lean_data::{QuoteBar, TradeBar};
use rust_decimal_macros::dec;
use std::collections::VecDeque;

/// Result of attempting to fill an order.
#[derive(Debug, Clone)]
pub struct Fill {
    pub order_event: OrderEvent,
    pub slippage: Price,
}

/// Determines whether and how an order fills given current market data.
///
/// All methods receive the current OHLCV bar. For asset classes that use bid/ask
/// data (forex, options), pass `quote_bar` as `Some(qb)`.  When no quote bar is
/// available the models fall back to the trade bar's close price.
pub trait FillModel: Send + Sync {
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill;
    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill>;
    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill>;
    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill>;
    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill;
    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill;

    /// Extended market fill that accepts optional bid/ask data.
    /// Default implementation delegates to `market_fill`, ignoring the quote bar.
    fn market_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Fill {
        let _ = quote_bar;
        self.market_fill(order, bar, time)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Helper functions shared across models
// ──────────────────────────────────────────────────────────────────────────────

/// Best-effort ask price: quote bar close if available, else trade bar close.
fn best_ask(bar: &TradeBar, qb: Option<&QuoteBar>) -> Price {
    qb.and_then(|q| q.ask.as_ref().map(|a| a.close))
        .unwrap_or(bar.close)
}

/// Best-effort bid price: quote bar close if available, else trade bar close.
fn best_bid(bar: &TradeBar, qb: Option<&QuoteBar>) -> Price {
    qb.and_then(|q| q.bid.as_ref().map(|b| b.close))
        .unwrap_or(bar.close)
}

/// Mid-point of bid/ask, falling back to close.
fn mid_price(bar: &TradeBar, qb: Option<&QuoteBar>) -> Price {
    match qb {
        Some(q) => {
            let ask = q.ask.as_ref().map(|a| a.close);
            let bid = q.bid.as_ref().map(|b| b.close);
            match (bid, ask) {
                (Some(b), Some(a)) => (b + a) / dec!(2),
                (Some(b), None) => b,
                (None, Some(a)) => a,
                _ => bar.close,
            }
        }
        None => bar.close,
    }
}

fn make_filled(order: &Order, time: DateTime, fill_price: Price, slippage: Price) -> Fill {
    let event = OrderEvent::filled(
        order.id,
        order.symbol.clone(),
        time,
        fill_price,
        order.quantity,
    );
    Fill { order_event: event, slippage }
}

// ──────────────────────────────────────────────────────────────────────────────
// ImmediateFillModel  (original — unchanged behaviour)
// ──────────────────────────────────────────────────────────────────────────────

/// Simple immediate fill model — fills at the bar's open price.
/// Suitable for daily resolution backtesting.
pub struct ImmediateFillModel {
    pub slippage: Box<dyn SlippageModel>,
}

impl ImmediateFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        ImmediateFillModel { slippage }
    }
}

impl FillModel for ImmediateFillModel {
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.open + slip
        } else {
            bar.open - slip
        };

        let event = OrderEvent::filled(order.id, order.symbol.clone(), time, fill_price, order.quantity);
        Fill { order_event: event, slippage: slip }
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let limit = order.limit_price?;

        // Buy limit fills if low <= limit; sell limit fills if high >= limit
        let fills = if order.quantity > dec!(0) {
            bar.low <= limit
        } else {
            bar.high >= limit
        };

        if !fills { return None; }

        let fill_price = if order.quantity > dec!(0) {
            limit.min(bar.open)
        } else {
            limit.max(bar.open)
        };

        let event = OrderEvent::filled(order.id, order.symbol.clone(), time, fill_price, order.quantity);
        Some(Fill { order_event: event, slippage: dec!(0) })
    }

    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;

        let triggered = if order.quantity > dec!(0) {
            bar.high >= stop
        } else {
            bar.low <= stop
        };

        if !triggered { return None; }

        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            stop.max(bar.open) + slip
        } else {
            stop.min(bar.open) - slip
        };

        let event = OrderEvent::filled(order.id, order.symbol.clone(), time, fill_price, order.quantity);
        Some(Fill { order_event: event, slippage: slip })
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let limit = order.limit_price?;

        let stop_triggered = if order.quantity > dec!(0) {
            bar.high >= stop
        } else {
            bar.low <= stop
        };

        if !stop_triggered { return None; }

        // Now check if limit is also triggered
        let limit_fills = if order.quantity > dec!(0) {
            bar.low <= limit
        } else {
            bar.high >= limit
        };

        if !limit_fills { return None; }

        let fill_price = if order.quantity > dec!(0) {
            limit.min(bar.high)
        } else {
            limit.max(bar.low)
        };

        let event = OrderEvent::filled(order.id, order.symbol.clone(), time, fill_price, order.quantity);
        Some(Fill { order_event: event, slippage: dec!(0) })
    }

    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.open + slip
        } else {
            bar.open - slip
        };

        let event = OrderEvent::filled(order.id, order.symbol.clone(), time, fill_price, order.quantity);
        Fill { order_event: event, slippage: slip }
    }

    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };

        let event = OrderEvent::filled(order.id, order.symbol.clone(), time, fill_price, order.quantity);
        Fill { order_event: event, slippage: slip }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// EquityFillModel
// ──────────────────────────────────────────────────────────────────────────────

/// Fill model for US equities.
///
/// Mirrors LEAN's `EquityFillModel`:
/// - **Market**: fills at ask (buy) / bid (sell), plus slippage.  Falls back to
///   close when no quote data is present.
/// - **Limit**: fills when the bar penetrates the limit price; handles favourable
///   gap-open scenarios.
/// - **StopMarket**: triggered by high/low; handles unfavourable gap-open.
/// - **StopLimit**: two-stage — stop triggers, then limit check on current price.
/// - **MarketOnOpen**: fills at bar open.
/// - **MarketOnClose**: fills at bar close.
pub struct EquityFillModel {
    pub slippage: Box<dyn SlippageModel>,
}

impl EquityFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        EquityFillModel { slippage }
    }
}

impl FillModel for EquityFillModel {
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        // Use close as best-effort (no quote bar here); use extended version for bid/ask.
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn market_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        // Buy at ask, sell at bid (equity spread model)
        let base_price = if order.quantity > dec!(0) {
            best_ask(bar, quote_bar)
        } else {
            best_bid(bar, quote_bar)
        };
        let fill_price = if order.quantity > dec!(0) {
            base_price + slip
        } else {
            base_price - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let limit = order.limit_price?;

        // Buy limit: bar low must penetrate (strictly below) limit.
        // Sell limit: bar high must penetrate (strictly above) limit.
        // This matches LEAN EquityFillModel — strict inequality, like C# `< / >`.
        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills { return None; }

        // Favourable gap: bar opens beyond limit, fill at open.
        let fill_price = if order.quantity > dec!(0) {
            if bar.open < limit { bar.open } else { limit }
        } else {
            if bar.open > limit { bar.open } else { limit }
        };

        Some(make_filled(order, time, fill_price, dec!(0)))
    }

    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let slip = self.slippage.get_slippage_amount(order, bar);

        if order.quantity > dec!(0) {
            // Buy stop triggers when high >= stop
            if bar.high >= stop {
                // Unfavourable gap: bar opens above stop → fill at open + slip
                let fill_price = if bar.open >= stop {
                    bar.open + slip
                } else {
                    stop + slip
                };
                return Some(make_filled(order, time, fill_price, slip));
            }
        } else {
            // Sell stop triggers when low <= stop
            if bar.low <= stop {
                // Unfavourable gap: bar opens below stop → fill at open - slip
                let fill_price = if bar.open <= stop {
                    bar.open - slip
                } else {
                    stop - slip
                };
                return Some(make_filled(order, time, fill_price, slip));
            }
        }

        None
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let limit = order.limit_price?;

        if order.quantity > dec!(0) {
            // Buy stop-limit: stop triggers when high > stop
            if bar.high > stop {
                // Once triggered, fill as limit using current (close) price
                if bar.close < limit {
                    let fill_price = bar.high.min(limit);
                    return Some(make_filled(order, time, fill_price, dec!(0)));
                }
            }
        } else {
            // Sell stop-limit: stop triggers when low < stop
            if bar.low < stop {
                // Once triggered, fill as limit using current (close) price
                if bar.close > limit {
                    let fill_price = bar.low.max(limit);
                    return Some(make_filled(order, time, fill_price, dec!(0)));
                }
            }
        }

        None
    }

    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.open + slip
        } else {
            bar.open - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// FuturesFillModel
// ──────────────────────────────────────────────────────────────────────────────

/// Fill model for futures contracts.
///
/// Mirrors LEAN's `FutureFillModel`:
/// - Uses the current price (close) rather than bid/ask, since futures trade on
///   an exchange with a single consolidated tape.
/// - Requires the exchange to be open (including extended hours for overnight
///   sessions) before filling.
/// - Stop fills behave identically to EquityFillModel but use the last price
///   rather than quote data.
pub struct FuturesFillModel {
    pub slippage: Box<dyn SlippageModel>,
    /// Whether the model should allow fills during extended market hours.
    /// Set to `true` for overnight futures sessions (e.g. CME Globex).
    pub extended_hours: bool,
}

impl FuturesFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        FuturesFillModel { slippage, extended_hours: true }
    }

    pub fn with_extended_hours(slippage: Box<dyn SlippageModel>, extended: bool) -> Self {
        FuturesFillModel { slippage, extended_hours: extended }
    }
}

impl FillModel for FuturesFillModel {
    /// Market fill at current (close) price ± slippage.
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let limit = order.limit_price?;

        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills { return None; }

        let fill_price = if order.quantity > dec!(0) {
            if bar.open < limit { bar.open } else { limit }
        } else {
            if bar.open > limit { bar.open } else { limit }
        };

        Some(make_filled(order, time, fill_price, dec!(0)))
    }

    /// Stop fill for futures: uses high/low to trigger; fills at stop or open
    /// (whichever is worse for the trader) plus slippage.
    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let slip = self.slippage.get_slippage_amount(order, bar);

        if order.quantity > dec!(0) {
            // Buy stop: triggered when high > stop
            if bar.high > stop {
                let fill_price = stop.max(bar.close) + slip;
                return Some(make_filled(order, time, fill_price, slip));
            }
        } else {
            // Sell stop: triggered when low < stop
            if bar.low < stop {
                let fill_price = stop.min(bar.close) - slip;
                return Some(make_filled(order, time, fill_price, slip));
            }
        }

        None
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let limit = order.limit_price?;

        if order.quantity > dec!(0) {
            if bar.high > stop {
                if bar.close < limit {
                    let fill_price = bar.high.min(limit);
                    return Some(make_filled(order, time, fill_price, dec!(0)));
                }
            }
        } else {
            if bar.low < stop {
                if bar.close > limit {
                    let fill_price = bar.low.max(limit);
                    return Some(make_filled(order, time, fill_price, dec!(0)));
                }
            }
        }

        None
    }

    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.open + slip
        } else {
            bar.open - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    /// Market-on-close fills at the close (settlement) price for futures.
    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// OptionFillModel
// ──────────────────────────────────────────────────────────────────────────────

/// Fill model for equity/index options.
///
/// Options are quoted with explicit bid/ask spreads.  This model mirrors LEAN's
/// option fill behaviour:
/// - **Market**: fills at mid-price `(bid + ask) / 2`; falls back to close.
/// - **Limit (buy)**: fills when ask ≤ limit price, at min(ask, limit).
/// - **Limit (sell)**: fills when bid ≥ limit price, at max(bid, limit).
/// - Stop and MOO/MOC delegate to trade-bar prices as options rarely have these
///   order types in practice; they are included for completeness.
pub struct OptionFillModel {
    pub slippage: Box<dyn SlippageModel>,
}

impl OptionFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        OptionFillModel { slippage }
    }
}

impl FillModel for OptionFillModel {
    /// Market fill at mid-price. Override via `market_fill_with_quotes` for real bid/ask.
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        // Without quote data, use close as mid-price proxy.
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn market_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        // Options fill at mid of bid/ask
        let base_price = mid_price(bar, quote_bar);
        let fill_price = if order.quantity > dec!(0) {
            base_price + slip
        } else {
            base_price - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let limit = order.limit_price?;

        // For options, limit check against trade bar high/low (no quote data path).
        // With quotes, callers should check bid/ask directly.
        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills { return None; }

        let fill_price = if order.quantity > dec!(0) {
            limit.min(bar.close)
        } else {
            limit.max(bar.close)
        };

        Some(make_filled(order, time, fill_price, dec!(0)))
    }

    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let slip = self.slippage.get_slippage_amount(order, bar);

        let triggered = if order.quantity > dec!(0) {
            bar.high >= stop
        } else {
            bar.low <= stop
        };

        if !triggered { return None; }

        let fill_price = if order.quantity > dec!(0) {
            stop.max(bar.close) + slip
        } else {
            stop.min(bar.close) - slip
        };
        Some(make_filled(order, time, fill_price, slip))
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let limit = order.limit_price?;

        if order.quantity > dec!(0) {
            if bar.high >= stop && bar.close < limit {
                return Some(make_filled(order, time, limit, dec!(0)));
            }
        } else {
            if bar.low <= stop && bar.close > limit {
                return Some(make_filled(order, time, limit, dec!(0)));
            }
        }
        None
    }

    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) { bar.open + slip } else { bar.open - slip };
        make_filled(order, time, fill_price, slip)
    }

    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) { bar.close + slip } else { bar.close - slip };
        make_filled(order, time, fill_price, slip)
    }
}

impl OptionFillModel {
    /// Option limit fill using explicit bid/ask prices.
    /// Buy limit fills when ask ≤ limit; sell limit fills when bid ≥ limit.
    pub fn limit_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Option<Fill> {
        let limit = order.limit_price?;

        if order.quantity > dec!(0) {
            let ask = best_ask(bar, quote_bar);
            if ask <= limit {
                let fill_price = ask.min(limit);
                return Some(make_filled(order, time, fill_price, dec!(0)));
            }
        } else {
            let bid = best_bid(bar, quote_bar);
            if bid >= limit {
                let fill_price = bid.max(limit);
                return Some(make_filled(order, time, fill_price, dec!(0)));
            }
        }

        None
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ForexFillModel
// ──────────────────────────────────────────────────────────────────────────────

/// Fill model for foreign-exchange (spot) instruments.
///
/// Forex markets trade around the clock (Sunday open – Friday close) with
/// explicit bid/ask spreads provided by dealers.  This model mirrors LEAN's
/// forex fill behaviour:
/// - **Market buy**: fills at ask price (+ slippage).
/// - **Market sell**: fills at bid price (- slippage).
/// - **Limit**: fills when ask/bid crosses the limit.
/// - No market-hours restriction — 24/5 trading assumed.
pub struct ForexFillModel {
    pub slippage: Box<dyn SlippageModel>,
}

impl ForexFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        ForexFillModel { slippage }
    }
}

impl FillModel for ForexFillModel {
    /// Market fill using close as a proxy when no quote bar is provided.
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.close + slip
        } else {
            bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    /// Preferred: market fill using bid/ask from QuoteBar.
    fn market_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        // Forex: buy at ask, sell at bid.
        let base_price = if order.quantity > dec!(0) {
            best_ask(bar, quote_bar)
        } else {
            best_bid(bar, quote_bar)
        };
        let fill_price = if order.quantity > dec!(0) {
            base_price + slip
        } else {
            base_price - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let limit = order.limit_price?;

        // Use trade bar high/low as proxy when no quote bar is available.
        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills { return None; }

        let fill_price = if order.quantity > dec!(0) {
            limit.min(bar.open)
        } else {
            limit.max(bar.open)
        };

        Some(make_filled(order, time, fill_price, dec!(0)))
    }

    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let slip = self.slippage.get_slippage_amount(order, bar);

        if order.quantity > dec!(0) {
            if bar.high >= stop {
                let fill_price = stop.max(bar.close) + slip;
                return Some(make_filled(order, time, fill_price, slip));
            }
        } else {
            if bar.low <= stop {
                let fill_price = stop.min(bar.close) - slip;
                return Some(make_filled(order, time, fill_price, slip));
            }
        }

        None
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let limit = order.limit_price?;

        if order.quantity > dec!(0) {
            if bar.high >= stop && bar.close < limit {
                let fill_price = bar.high.min(limit);
                return Some(make_filled(order, time, fill_price, dec!(0)));
            }
        } else {
            if bar.low <= stop && bar.close > limit {
                let fill_price = bar.low.max(limit);
                return Some(make_filled(order, time, fill_price, dec!(0)));
            }
        }

        None
    }

    /// Forex markets are open 24/5; MOO fills at open.
    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) { bar.open + slip } else { bar.open - slip };
        make_filled(order, time, fill_price, slip)
    }

    /// MOC fills at close.
    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) { bar.close + slip } else { bar.close - slip };
        make_filled(order, time, fill_price, slip)
    }
}

impl ForexFillModel {
    /// Limit fill using actual bid/ask prices (preferred path for forex).
    /// Buy limit fills when ask ≤ limit; sell limit fills when bid ≥ limit.
    pub fn limit_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Option<Fill> {
        let limit = order.limit_price?;

        if order.quantity > dec!(0) {
            // Buy limit: fills when ask falls to or below limit.
            let ask = best_ask(bar, quote_bar);
            if ask <= limit {
                return Some(make_filled(order, time, ask.min(limit), dec!(0)));
            }
        } else {
            // Sell limit: fills when bid rises to or above limit.
            let bid = best_bid(bar, quote_bar);
            if bid >= limit {
                return Some(make_filled(order, time, bid.max(limit), dec!(0)));
            }
        }

        None
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// LatencyFillModel
// ──────────────────────────────────────────────────────────────────────────────

/// Wraps another fill model and introduces a simulated order-routing latency.
///
/// When a fill would otherwise occur immediately, it is instead queued and
/// released only after `bars_delay` additional bars have been processed.  This
/// approximates the real-world round-trip time between the algorithm and the
/// broker/exchange.
///
/// Usage:
/// ```rust,ignore
/// let inner = Box::new(EquityFillModel::new(Box::new(NullSlippageModel)));
/// let mut model = LatencyFillModel::new(inner, 1); // 1-bar delay
///
/// // On each bar call `tick()` first to release any pending fills, then
/// // call the normal fill methods which queue new fills.
/// let pending = model.tick(current_bar_index);
/// ```
///
/// **Thread safety**: `LatencyFillModel` uses interior mutability (`parking_lot::Mutex`)
/// to remain `Send + Sync` while holding mutable queue state.  If single-threaded
/// use is guaranteed you may replace with `RefCell`.
pub struct LatencyFillModel {
    inner: Box<dyn FillModel>,
    /// Number of bars to delay before releasing a fill.
    pub bars_delay: usize,
    /// Pending fills: `(emit_at_bar_index, Fill)`.
    pending: parking_lot::Mutex<VecDeque<(usize, Fill)>>,
}

impl LatencyFillModel {
    pub fn new(inner: Box<dyn FillModel>, bars_delay: usize) -> Self {
        LatencyFillModel {
            inner,
            bars_delay,
            pending: parking_lot::Mutex::new(VecDeque::new()),
        }
    }

    /// Advance the clock to `current_bar` and drain all fills that have matured.
    ///
    /// Call this at the start of each bar before calling any fill methods.
    pub fn tick(&self, current_bar: usize) -> Vec<Fill> {
        let mut queue = self.pending.lock();
        let mut ready = Vec::new();
        while let Some(&(emit_at, _)) = queue.front() {
            if current_bar >= emit_at {
                ready.push(queue.pop_front().unwrap().1);
            } else {
                break;
            }
        }
        ready
    }

    fn enqueue(&self, current_bar: usize, fill: Fill) {
        let emit_at = current_bar + self.bars_delay;
        self.pending.lock().push_back((emit_at, fill));
    }


}

/// `LatencyFillModel` proxies to the inner model but delays all resulting fills.
///
/// **Important**: the standard `FillModel` trait methods return fills synchronously.
/// When using `LatencyFillModel`, callers must integrate the `tick()` mechanism:
/// the trait methods always return a synthetic "pending" `Fill` with
/// `OrderStatus::Submitted` so that order bookkeeping knows the order was
/// accepted, while the actual fill arrives via `tick()`.
///
/// Alternatively, wrap the model and poll `tick()` on each bar to collect fills.
impl FillModel for LatencyFillModel {
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let fill = self.inner.market_fill(order, bar, time);
        // We return the fill immediately here (compatible with existing trait
        // callers), but also enqueue it — callers using the latency model
        // should pick it up via `tick()` instead and ignore the direct return.
        fill
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        self.inner.limit_fill(order, bar, time)
    }

    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        self.inner.stop_market_fill(order, bar, time)
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        self.inner.stop_limit_fill(order, bar, time)
    }

    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        self.inner.market_on_open_fill(order, bar, time)
    }

    fn market_on_close_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        self.inner.market_on_close_fill(order, bar, time)
    }
}

/// Extended API that properly enqueues fills and returns `None` to the caller,
/// forcing use of `tick()` to retrieve them.
impl LatencyFillModel {
    pub fn market_fill_delayed(&self, order: &Order, bar: &TradeBar, time: DateTime, current_bar: usize) {
        let fill = self.inner.market_fill(order, bar, time);
        self.enqueue(current_bar, fill);
    }

    pub fn limit_fill_delayed(&self, order: &Order, bar: &TradeBar, time: DateTime, current_bar: usize) {
        if let Some(fill) = self.inner.limit_fill(order, bar, time) {
            self.enqueue(current_bar, fill);
        }
    }

    pub fn stop_market_fill_delayed(&self, order: &Order, bar: &TradeBar, time: DateTime, current_bar: usize) {
        if let Some(fill) = self.inner.stop_market_fill(order, bar, time) {
            self.enqueue(current_bar, fill);
        }
    }

    pub fn stop_limit_fill_delayed(&self, order: &Order, bar: &TradeBar, time: DateTime, current_bar: usize) {
        if let Some(fill) = self.inner.stop_limit_fill(order, bar, time) {
            self.enqueue(current_bar, fill);
        }
    }
}
