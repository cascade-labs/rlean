use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::order::{Order, OrderDirection, OrderType};

/// A limit-if-touched order.
///
/// Once a `trigger_price` is touched, a limit order at `limit_price` is activated.
/// This is the inverse of a stop-limit: the trigger fires when the price moves
/// *toward* the limit, not away from it.
///
/// - For a **buy** order: trigger fires when market price falls *at or below* the
///   trigger price (price came down to an attractive level), then the limit order
///   is placed to buy at `limit_price`.
/// - For a **sell** order: trigger fires when market price rises *at or above* the
///   trigger price, then the limit order is placed to sell at `limit_price`.
///
/// Mirrors C# `LimitIfTouchedOrder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitIfTouchedOrder {
    pub order: Order,
    /// The price that, when touched, activates the underlying limit order.
    pub trigger_price: Price,
    /// The limit price for the order once the trigger is touched.
    pub limit_price: Price,
    /// Whether the trigger has already been touched.
    pub trigger_touched: bool,
}

impl LimitIfTouchedOrder {
    /// Create a new limit-if-touched order.
    pub fn new(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        trigger_price: Price,
        limit_price: Price,
        time: DateTime,
        tag: &str,
    ) -> Self {
        let mut order = Order::market(id, symbol, quantity, time, tag);
        order.order_type = OrderType::LimitIfTouched;
        order.stop_price = Some(trigger_price);   // reuse stop_price slot for trigger
        order.limit_price = Some(limit_price);
        Self {
            order,
            trigger_price,
            limit_price,
            trigger_touched: false,
        }
    }

    /// Check whether `market_price` touches the trigger. Marks the order as
    /// triggered and returns `true` on the first touch; subsequent calls return
    /// `false` (the trigger cannot fire twice).
    pub fn check_trigger(&mut self, market_price: Price) -> bool {
        if self.trigger_touched {
            return false;
        }
        let touched = match self.order.direction() {
            // buy: trigger when price drops to or below trigger
            OrderDirection::Buy => market_price <= self.trigger_price,
            // sell (and Hold): trigger when price rises to or above trigger
            _ => market_price >= self.trigger_price,
        };
        if touched {
            self.trigger_touched = true;
        }
        touched
    }

    /// After the trigger is touched, returns `true` if `market_price` would
    /// result in a limit fill (i.e., limit conditions are met).
    pub fn would_fill(&self, market_price: Price) -> bool {
        if !self.trigger_touched {
            return false;
        }
        match self.order.direction() {
            OrderDirection::Buy => market_price <= self.limit_price,
            _ => market_price >= self.limit_price,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{DateTime, Symbol};
    use rust_decimal_macros::dec;

    fn make_lit(qty: Quantity, trigger: Price, limit: Price) -> LimitIfTouchedOrder {
        let symbol = Symbol::create_equity("AAPL", &lean_core::Market::usa());
        LimitIfTouchedOrder::new(1, symbol, qty, trigger, limit, DateTime::EPOCH, "test")
    }

    #[test]
    fn buy_trigger_fires_when_price_drops() {
        let mut order = make_lit(dec!(10), dec!(95), dec!(94));
        assert!(!order.check_trigger(dec!(96))); // above trigger, no fire
        assert!(order.check_trigger(dec!(95)));  // at trigger, fires
        assert!(!order.check_trigger(dec!(94))); // already triggered
    }

    #[test]
    fn sell_trigger_fires_when_price_rises() {
        let mut order = make_lit(dec!(-10), dec!(105), dec!(106));
        assert!(!order.check_trigger(dec!(104)));
        assert!(order.check_trigger(dec!(105)));
    }

    #[test]
    fn would_fill_after_trigger() {
        let mut order = make_lit(dec!(10), dec!(95), dec!(94));
        assert!(!order.would_fill(dec!(93))); // not yet triggered
        order.check_trigger(dec!(95));
        assert!(order.would_fill(dec!(94)));  // at limit
        assert!(order.would_fill(dec!(90)));  // below limit
        assert!(!order.would_fill(dec!(96))); // above limit
    }
}
