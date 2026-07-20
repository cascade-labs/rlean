use crate::{order::Order, order_event::OrderEvent, slippage::SlippageModel};
use rlean_core::{DateTime, Price};
use rlean_data_tables::{QuoteBar, TradeBar, TradeBarData};
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

    /// Fill from a QuoteBar when no TradeBar subscription exists.
    ///
    /// C# LEAN's fill models read the security cache and can price an order from
    /// quote data alone. The directional quote side is promoted to the common
    /// OHLC input used by rlean fill models: buys consume ask OHLC and sells
    /// consume bid OHLC.
    fn market_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<Fill> {
        let bar = directional_trade_bar_from_quote(order, quote_bar)?;
        Some(self.market_fill_with_quotes(order, &bar, Some(quote_bar), time))
    }

    fn limit_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Option<Fill> {
        if time <= order.time {
            return None;
        }
        if let Some(bar) = directional_quote_trade_bar(order, bar, quote_bar) {
            self.limit_fill(order, &bar, time)
        } else {
            self.limit_fill(order, bar, time)
        }
    }

    fn limit_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<Fill> {
        let bar = directional_trade_bar_from_quote(order, quote_bar)?;
        self.limit_fill_with_quotes(order, &bar, Some(quote_bar), time)
    }

    fn stop_market_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Option<Fill> {
        if let Some(bar) = directional_quote_trade_bar(order, bar, quote_bar) {
            self.stop_market_fill(order, &bar, time)
        } else {
            self.stop_market_fill(order, bar, time)
        }
    }

    fn stop_market_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<Fill> {
        let bar = directional_trade_bar_from_quote(order, quote_bar)?;
        self.stop_market_fill_with_quotes(order, &bar, Some(quote_bar), time)
    }

    fn stop_limit_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Option<Fill> {
        if let Some(bar) = directional_quote_trade_bar(order, bar, quote_bar) {
            self.stop_limit_fill(order, &bar, time)
        } else {
            self.stop_limit_fill(order, bar, time)
        }
    }

    fn stop_limit_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<Fill> {
        let bar = directional_trade_bar_from_quote(order, quote_bar)?;
        self.stop_limit_fill_with_quotes(order, &bar, Some(quote_bar), time)
    }

    fn market_on_open_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Fill {
        if let Some(bar) = directional_quote_trade_bar(order, bar, quote_bar) {
            self.market_on_open_fill(order, &bar, time)
        } else {
            self.market_on_open_fill(order, bar, time)
        }
    }

    fn market_on_open_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<Fill> {
        let bar = directional_trade_bar_from_quote(order, quote_bar)?;
        Some(self.market_on_open_fill_with_quotes(order, &bar, Some(quote_bar), time))
    }

    fn market_on_close_fill_with_quotes(
        &self,
        order: &Order,
        bar: &TradeBar,
        quote_bar: Option<&QuoteBar>,
        time: DateTime,
    ) -> Fill {
        if let Some(bar) = directional_quote_trade_bar(order, bar, quote_bar) {
            self.market_on_close_fill(order, &bar, time)
        } else {
            self.market_on_close_fill(order, bar, time)
        }
    }

    fn market_on_close_fill_from_quote(
        &self,
        order: &Order,
        quote_bar: &QuoteBar,
        time: DateTime,
    ) -> Option<Fill> {
        let bar = directional_trade_bar_from_quote(order, quote_bar)?;
        Some(self.market_on_close_fill_with_quotes(order, &bar, Some(quote_bar), time))
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

fn directional_quote_trade_bar(
    order: &Order,
    fallback: &TradeBar,
    quote_bar: Option<&QuoteBar>,
) -> Option<TradeBar> {
    let quote_bar = quote_bar?;
    let mut bar = directional_trade_bar_from_quote(order, quote_bar)?;
    bar.volume = fallback.volume;
    Some(bar)
}

fn directional_trade_bar_from_quote(order: &Order, quote_bar: &QuoteBar) -> Option<TradeBar> {
    let side = if order.quantity > dec!(0) {
        quote_bar.ask.as_ref().or(quote_bar.bid.as_ref())
    } else {
        quote_bar.bid.as_ref().or(quote_bar.ask.as_ref())
    }?;

    Some(TradeBar::new(
        order.symbol.clone(),
        quote_bar.time,
        quote_bar.period,
        TradeBarData::new(
            side.open,
            side.high,
            side.low,
            side.close,
            quote_bar.last_bid_size + quote_bar.last_ask_size,
        ),
    ))
}

fn make_filled(order: &Order, time: DateTime, fill_price: Price, slippage: Price) -> Fill {
    let mut event = OrderEvent::filled(
        order.id,
        order.symbol.clone(),
        time,
        fill_price,
        order.quantity,
    );
    event.apply_order_fields(order);
    Fill {
        order_event: event,
        slippage,
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// ImmediateFillModel
// ──────────────────────────────────────────────────────────────────────────────

/// Immediate fill model.
///
/// With trade bars only, market orders fill at the bar close/current price.
/// When quote bars are supplied, fills use the order-direction side of the
/// quote bar: buys use ask prices and sells use bid prices. This mirrors
/// LEAN's `GetPrices` selection model for subscribed quote data while
/// preserving the trade-bar path.
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
        let Some(quote_bar) = directional_quote_trade_bar(order, bar, quote_bar) else {
            return self.market_fill(order, bar, time);
        };

        let slip = self.slippage.get_slippage_amount(order, &quote_bar);
        let fill_price = if order.quantity > dec!(0) {
            quote_bar.close + slip
        } else {
            quote_bar.close - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    fn limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let limit = order.limit_price?;

        // Match LEAN's base FillModel: a limit order fills only after the bar
        // penetrates the limit price, not merely when it touches it.
        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills {
            return None;
        }

        let fill_price = if order.quantity > dec!(0) {
            limit.min(bar.open)
        } else {
            limit.max(bar.open)
        };

        Some(make_filled(order, time, fill_price, dec!(0)))
    }

    fn stop_market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;

        let triggered = if order.quantity > dec!(0) {
            bar.high >= stop
        } else {
            bar.low <= stop
        };

        if !triggered {
            return None;
        }

        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            stop.max(bar.open) + slip
        } else {
            stop.min(bar.open) - slip
        };

        Some(make_filled(order, time, fill_price, slip))
    }

    fn stop_limit_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Option<Fill> {
        let stop = order.stop_price?;
        let limit = order.limit_price?;

        let stop_triggered = if order.quantity > dec!(0) {
            bar.high >= stop
        } else {
            bar.low <= stop
        };

        if !stop_triggered {
            return None;
        }

        // Now check if limit is also triggered. Use the same strict
        // penetration semantics as limit orders.
        let limit_fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !limit_fills {
            return None;
        }

        let fill_price = if order.quantity > dec!(0) {
            limit.min(bar.high)
        } else {
            limit.max(bar.low)
        };

        Some(make_filled(order, time, fill_price, dec!(0)))
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

        if !fills {
            return None;
        }

        // Favourable gap: bar opens beyond limit, fill at open.
        let fill_price = if order.quantity > dec!(0) {
            if bar.open < limit {
                bar.open
            } else {
                limit
            }
        } else if bar.open > limit {
            bar.open
        } else {
            limit
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
/// Mirrors LEAN's base `FillModel.GetPrices`:
/// - **Market (buy)**: fills at ask price (+ slippage) when quote data is
///   available; falls back to the trade-bar close otherwise.
/// - **Market (sell)**: fills at bid price (- slippage) when quote data is
///   available; falls back to the trade-bar close otherwise.
/// - Requires the exchange to be open (including extended hours for overnight
///   sessions) before filling.
/// - Stop and MOO/MOC use the order-direction quote side via the trait-level
///   quote-aware paths (`*_with_quotes`), falling back to trade-bar prices.
pub struct FuturesFillModel {
    pub slippage: Box<dyn SlippageModel>,
    /// Whether the model should allow fills during extended market hours.
    /// Set to `true` for overnight futures sessions (e.g. CME Globex).
    pub extended_hours: bool,
}

impl FuturesFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        FuturesFillModel {
            slippage,
            extended_hours: true,
        }
    }

    pub fn with_extended_hours(slippage: Box<dyn SlippageModel>, extended: bool) -> Self {
        FuturesFillModel {
            slippage,
            extended_hours: extended,
        }
    }
}

impl FillModel for FuturesFillModel {
    /// Market fill using close as best-effort (no quote bar here); use the
    /// extended `market_fill_with_quotes` for real bid/ask.
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
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
        // Buy at ask, sell at bid; fall back to close when no quote data.
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

        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills {
            return None;
        }

        let fill_price = if order.quantity > dec!(0) {
            if bar.open < limit {
                bar.open
            } else {
                limit
            }
        } else if bar.open > limit {
            bar.open
        } else {
            limit
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
            if bar.high > stop && bar.close < limit {
                let fill_price = bar.high.min(limit);
                return Some(make_filled(order, time, fill_price, dec!(0)));
            }
        } else if bar.low < stop && bar.close > limit {
            let fill_price = bar.low.max(limit);
            return Some(make_filled(order, time, fill_price, dec!(0)));
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
/// base `FillModel.GetPrices`, which fills at the order-direction quote side
/// whenever quote data is subscribed:
/// - **Market (buy)**: fills at ask price (+ slippage); falls back to close.
/// - **Market (sell)**: fills at bid price (- slippage); falls back to close.
/// - **Limit (buy)**: fills when ask ≤ limit price, at min(ask, limit).
/// - **Limit (sell)**: fills when bid ≥ limit price, at max(bid, limit).
/// - Stop and MOO/MOC use the order-direction quote side via the trait-level
///   quote-aware paths (`*_with_quotes`), falling back to trade-bar prices.
pub struct OptionFillModel {
    pub slippage: Box<dyn SlippageModel>,
}

impl OptionFillModel {
    pub fn new(slippage: Box<dyn SlippageModel>) -> Self {
        OptionFillModel { slippage }
    }
}

impl FillModel for OptionFillModel {
    /// Market fill using close as best-effort (no quote bar here); use the
    /// extended `market_fill_with_quotes` for real bid/ask.
    fn market_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
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
        // Buy at ask, sell at bid; fall back to close when no quote data.
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

        // For options, limit check against trade bar high/low. The trait-level
        // quote-aware path first converts bid/ask quotes into directional bars.
        let fills = if order.quantity > dec!(0) {
            bar.low < limit
        } else {
            bar.high > limit
        };

        if !fills {
            return None;
        }

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

        if !triggered {
            return None;
        }

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
        } else if bar.low <= stop && bar.close > limit {
            return Some(make_filled(order, time, limit, dec!(0)));
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
/// base `FillModel.GetPrices` (quote-side fills):
/// - **Market buy**: fills at ask price (+ slippage); falls back to close.
/// - **Market sell**: fills at bid price (- slippage); falls back to close.
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

        if !fills {
            return None;
        }

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
        } else if bar.low <= stop {
            let fill_price = stop.min(bar.close) - slip;
            return Some(make_filled(order, time, fill_price, slip));
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
        } else if bar.low <= stop && bar.close > limit {
            let fill_price = bar.low.max(limit);
            return Some(make_filled(order, time, fill_price, dec!(0)));
        }

        None
    }

    /// Forex markets are open 24/5; MOO fills at open.
    fn market_on_open_fill(&self, order: &Order, bar: &TradeBar, time: DateTime) -> Fill {
        let slip = self.slippage.get_slippage_amount(order, bar);
        let fill_price = if order.quantity > dec!(0) {
            bar.open + slip
        } else {
            bar.open - slip
        };
        make_filled(order, time, fill_price, slip)
    }

    /// MOC fills at close.
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
        // We return the fill immediately here (compatible with existing trait
        // callers), but also enqueue it — callers using the latency model
        // should pick it up via `tick()` instead and ignore the direct return.
        self.inner.market_fill(order, bar, time)
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
    pub fn market_fill_delayed(
        &self,
        order: &Order,
        bar: &TradeBar,
        time: DateTime,
        current_bar: usize,
    ) {
        let fill = self.inner.market_fill(order, bar, time);
        self.enqueue(current_bar, fill);
    }

    pub fn limit_fill_delayed(
        &self,
        order: &Order,
        bar: &TradeBar,
        time: DateTime,
        current_bar: usize,
    ) {
        if let Some(fill) = self.inner.limit_fill(order, bar, time) {
            self.enqueue(current_bar, fill);
        }
    }

    pub fn stop_market_fill_delayed(
        &self,
        order: &Order,
        bar: &TradeBar,
        time: DateTime,
        current_bar: usize,
    ) {
        if let Some(fill) = self.inner.stop_market_fill(order, bar, time) {
            self.enqueue(current_bar, fill);
        }
    }

    pub fn stop_limit_fill_delayed(
        &self,
        order: &Order,
        bar: &TradeBar,
        time: DateTime,
        current_bar: usize,
    ) {
        if let Some(fill) = self.inner.stop_limit_fill(order, bar, time) {
            self.enqueue(current_bar, fill);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::slippage::NullSlippageModel;
    use rlean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
    use rlean_data_tables::Bar;

    fn ts(i: i64) -> DateTime {
        NanosecondTimestamp::from_secs(i * 86400)
    }

    fn sym() -> Symbol {
        Symbol::create_equity("SPY", &Market::usa())
    }

    fn trade_bar(close: f64) -> TradeBar {
        TradeBar::new(
            sym(),
            ts(0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(
                Price::try_from(close).unwrap(),
                Price::try_from(close).unwrap(),
                Price::try_from(close).unwrap(),
                Price::try_from(close).unwrap(),
                dec!(100000),
            ),
        )
    }

    /// Quote bar with a `bid` close and an `ask` close (both flat OHLC).
    fn quote_bar(bid: f64, ask: f64) -> QuoteBar {
        let b = Price::try_from(bid).unwrap();
        let a = Price::try_from(ask).unwrap();
        QuoteBar::new(
            sym(),
            ts(0),
            TimeSpan::ONE_DAY,
            Some(Bar::new(b, b, b, b)),
            Some(Bar::new(a, a, a, a)),
            dec!(10),
            dec!(10),
        )
    }

    fn buy() -> Order {
        Order::market(1, sym(), dec!(10), ts(0), "")
    }

    fn sell() -> Order {
        Order::market(1, sym(), dec!(-10), ts(0), "")
    }

    // Each closure builds a fresh model with no slippage so fills equal the
    // raw quote side.  We test the trait entry point `market_fill_with_quotes`
    // that the order processor actually calls.
    macro_rules! quote_side_suite {
        ($name:ident, $model:expr) => {
            mod $name {
                use super::*;

                #[test]
                fn buy_fills_at_ask() {
                    let model = $model;
                    let bar = trade_bar(100.0);
                    let qb = quote_bar(99.0, 101.0);
                    let fill = model.market_fill_with_quotes(&buy(), &bar, Some(&qb), ts(0));
                    assert_eq!(
                        fill.order_event.fill_price,
                        dec!(101),
                        "market buy must fill at ask, not mid/last"
                    );
                }

                #[test]
                fn sell_fills_at_bid() {
                    let model = $model;
                    let bar = trade_bar(100.0);
                    let qb = quote_bar(99.0, 101.0);
                    let fill = model.market_fill_with_quotes(&sell(), &bar, Some(&qb), ts(0));
                    assert_eq!(
                        fill.order_event.fill_price,
                        dec!(99),
                        "market sell must fill at bid, not mid/last"
                    );
                }

                #[test]
                fn buy_never_fills_at_mid() {
                    let model = $model;
                    let bar = trade_bar(100.0);
                    let qb = quote_bar(99.0, 101.0);
                    let fill = model.market_fill_with_quotes(&buy(), &bar, Some(&qb), ts(0));
                    // mid = 100; ensure we are NOT filling there.
                    assert_ne!(fill.order_event.fill_price, dec!(100));
                }

                #[test]
                fn falls_back_to_close_without_quotes() {
                    let model = $model;
                    let bar = trade_bar(100.0);
                    let buy_fill = model.market_fill_with_quotes(&buy(), &bar, None, ts(0));
                    let sell_fill = model.market_fill_with_quotes(&sell(), &bar, None, ts(0));
                    assert_eq!(buy_fill.order_event.fill_price, dec!(100));
                    assert_eq!(sell_fill.order_event.fill_price, dec!(100));
                }
            }
        };
    }

    quote_side_suite!(
        immediate,
        ImmediateFillModel::new(Box::new(NullSlippageModel))
    );
    quote_side_suite!(equity, EquityFillModel::new(Box::new(NullSlippageModel)));
    quote_side_suite!(option, OptionFillModel::new(Box::new(NullSlippageModel)));
    quote_side_suite!(futures, FuturesFillModel::new(Box::new(NullSlippageModel)));
    quote_side_suite!(forex, ForexFillModel::new(Box::new(NullSlippageModel)));

    // Regression: options previously filled at mid (bid+ask)/2. Guard against
    // reintroducing that bug with an asymmetric spread.
    #[test]
    fn option_market_does_not_fill_at_mid_regression() {
        let model = OptionFillModel::new(Box::new(NullSlippageModel));
        let bar = trade_bar(5.0);
        // bid=1, ask=3 → mid=2. Buy must be 3, sell must be 1.
        let qb = quote_bar(1.0, 3.0);
        let buy_fill = model.market_fill_with_quotes(&buy(), &bar, Some(&qb), ts(0));
        let sell_fill = model.market_fill_with_quotes(&sell(), &bar, Some(&qb), ts(0));
        assert_eq!(buy_fill.order_event.fill_price, dec!(3));
        assert_eq!(sell_fill.order_event.fill_price, dec!(1));
        assert_ne!(buy_fill.order_event.fill_price, dec!(2));
        assert_ne!(sell_fill.order_event.fill_price, dec!(2));
    }

    // The order processor triggers stop orders through the `*_with_quotes`
    // trait paths, which build a directional trade bar from the quote side.
    // Verify a triggered buy stop respects the ask side.
    #[test]
    fn stop_market_buy_uses_ask_side_when_quotes_present() {
        let model = EquityFillModel::new(Box::new(NullSlippageModel));
        // Trade bar high 110 triggers a buy stop @105.
        let bar = TradeBar::new(
            sym(),
            ts(0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(110), dec!(90), dec!(105), dec!(1000)),
        );
        // Ask bar: high 112 (triggers), open 106.
        let ask = Bar::new(dec!(106), dec!(112), dec!(104), dec!(108));
        let bid = Bar::new(dec!(105), dec!(111), dec!(103), dec!(107));
        let qb = QuoteBar::new(
            sym(),
            ts(0),
            TimeSpan::ONE_DAY,
            Some(bid),
            Some(ask),
            dec!(10),
            dec!(10),
        );
        let order = Order::stop_market(1, sym(), dec!(10), dec!(105), ts(0), "");
        let fill = model
            .stop_market_fill_with_quotes(&order, &bar, Some(&qb), ts(0))
            .expect("buy stop should trigger");
        // Fill uses the ask bar: max(stop=105, ask.open=106) = 106.
        assert_eq!(fill.order_event.fill_price, dec!(106));
    }
}
