use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::order::{Order, OrderDirection, OrderType};

/// A trailing stop order that moves the stop price with the market.
///
/// For a sell (long protection) order, the stop rises with the market price but never falls.
/// For a buy (short protection) order, the stop falls with the market price but never rises.
///
/// Mirrors C# `TrailingStopOrder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrailingStopOrder {
    pub order: Order,
    /// The trailing amount — either a percentage (0.01 = 1%) or an absolute dollar value.
    pub trailing_amount: Price,
    /// When true, `trailing_amount` is a percentage of the current market price.
    /// When false, it is an absolute currency amount.
    pub trailing_as_percentage: bool,
    /// The current stop price. Updated as the market moves favorably.
    /// Zero means the stop price has not yet been initialized from a market price.
    pub stop_price: Price,
}

#[derive(Debug, Clone, Copy)]
pub struct TrailingStopOrderParams<'a> {
    pub trailing_amount: Price,
    pub trailing_as_percentage: bool,
    pub stop_price: Price,
    pub time: DateTime,
    pub tag: &'a str,
}

impl TrailingStopOrder {
    /// Create a new trailing stop order.
    ///
    /// `stop_price` may be zero if the initial stop should be computed on the first
    /// price update via [`update_stop_price`].
    pub fn new(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        params: TrailingStopOrderParams<'_>,
    ) -> Self {
        let mut order = Order::market(id, symbol, quantity, params.time, params.tag);
        order.order_type = OrderType::TrailingStop;
        order.trailing_amount = Some(params.trailing_amount);
        order.trailing_as_percent = params.trailing_as_percentage;
        order.stop_price = if params.stop_price.is_zero() {
            None
        } else {
            Some(params.stop_price)
        };
        Self {
            order,
            trailing_amount: params.trailing_amount,
            trailing_as_percentage: params.trailing_as_percentage,
            stop_price: params.stop_price,
        }
    }

    /// Calculate the stop price for a given market price and direction.
    ///
    /// Mirrors `TrailingStopOrder.CalculateStopPrice` in C#.
    pub fn calculate_stop_price(
        market_price: Price,
        trailing_amount: Price,
        trailing_as_percentage: bool,
        direction: OrderDirection,
    ) -> Price {
        if trailing_as_percentage {
            match direction {
                OrderDirection::Buy => market_price * (Decimal::ONE + trailing_amount),
                _ => market_price * (Decimal::ONE - trailing_amount),
            }
        } else {
            match direction {
                OrderDirection::Buy => market_price + trailing_amount,
                _ => market_price - trailing_amount,
            }
        }
    }

    /// Attempt to trail the stop price given a new market price.
    ///
    /// The stop is moved only when the market has moved favorably enough that
    /// the distance to the current stop exceeds the trailing amount — exactly
    /// mirroring `TrailingStopOrder.TryUpdateStopPrice` in C#.
    ///
    /// Returns `true` and updates `self.stop_price` if the stop moved.
    pub fn try_update_stop_price(&mut self, market_price: Price) -> bool {
        let direction = self.order.direction();

        // Initialize stop price if it hasn't been set yet.
        if self.stop_price.is_zero() {
            self.stop_price = Self::calculate_stop_price(
                market_price,
                self.trailing_amount,
                self.trailing_as_percentage,
                direction,
            );
            self.order.stop_price = Some(self.stop_price);
            return true;
        }

        let distance = match direction {
            OrderDirection::Sell => market_price - self.stop_price,
            _ => self.stop_price - market_price,
        };

        let stop_reference = if self.trailing_as_percentage {
            market_price * self.trailing_amount
        } else {
            self.trailing_amount
        };

        if distance <= stop_reference {
            return false;
        }

        let new_stop = Self::calculate_stop_price(
            market_price,
            self.trailing_amount,
            self.trailing_as_percentage,
            direction,
        );
        self.stop_price = new_stop;
        self.order.stop_price = Some(new_stop);
        true
    }

    /// Returns `true` if the current market price has triggered this stop.
    pub fn is_triggered(&self, market_price: Price) -> bool {
        if self.stop_price.is_zero() {
            return false;
        }
        match self.order.direction() {
            // sell stop: triggers when price falls to or below stop
            OrderDirection::Sell => market_price <= self.stop_price,
            // buy stop: triggers when price rises to or above stop
            _ => market_price >= self.stop_price,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{DateTime, Symbol};
    use rust_decimal_macros::dec;

    fn make_order(qty: Quantity, trail: Price, as_pct: bool) -> TrailingStopOrder {
        let symbol = Symbol::create_equity("AAPL", &lean_core::Market::usa());
        TrailingStopOrder::new(
            1,
            symbol,
            qty,
            TrailingStopOrderParams {
                trailing_amount: trail,
                trailing_as_percentage: as_pct,
                stop_price: dec!(0),
                time: DateTime::EPOCH,
                tag: "test",
            },
        )
    }

    #[test]
    fn sell_stop_initializes_and_trails() {
        // Sell order (negative qty) = trailing stop protecting a long position.
        // Stop is placed BELOW market price and rises as price rises.
        let mut order = make_order(dec!(-10), dec!(2), false); // sell stop, $2 trail
                                                               // First update at $100: stop = 100 - 2 = $98.
        assert!(order.try_update_stop_price(dec!(100)));
        assert_eq!(order.stop_price, dec!(98));

        // Price drops to $99: distance = 99 - 98 = 1 <= 2, no update.
        assert!(!order.try_update_stop_price(dec!(99)));
        assert_eq!(order.stop_price, dec!(98));

        // Price rises to $105: distance = 105 - 98 = 7 > 2, update to $103.
        assert!(order.try_update_stop_price(dec!(105)));
        assert_eq!(order.stop_price, dec!(103));
    }

    #[test]
    fn sell_stop_triggered() {
        // Sell stop triggers when market price falls to or below the stop.
        let mut order = make_order(dec!(-10), dec!(2), false);
        order.try_update_stop_price(dec!(100)); // stop = $98
        assert!(!order.is_triggered(dec!(99))); // 99 > 98, not triggered
        assert!(order.is_triggered(dec!(98))); // at stop
        assert!(order.is_triggered(dec!(95))); // below stop
    }

    #[test]
    fn buy_stop_initializes_and_trails() {
        // Buy order (positive qty) = trailing stop to cover a short position.
        // Stop is placed ABOVE market price and falls as price falls.
        let mut order = make_order(dec!(10), dec!(2), false); // buy stop, $2 trail
        assert!(order.try_update_stop_price(dec!(100)));
        assert_eq!(order.stop_price, dec!(102)); // 100 + 2

        // Price rises to $101: distance = 102 - 101 = 1 <= 2, no update.
        assert!(!order.try_update_stop_price(dec!(101)));

        // Price drops to $95: distance = 102 - 95 = 7 > 2, update to $97.
        assert!(order.try_update_stop_price(dec!(95)));
        assert_eq!(order.stop_price, dec!(97));
    }

    #[test]
    fn percentage_trailing() {
        // Sell stop, 5% trail
        let mut order = make_order(dec!(-10), dec!(0.05), true);
        order.try_update_stop_price(dec!(100));
        // stop = 100 * (1 - 0.05) = 95
        assert_eq!(order.stop_price, dec!(95));
    }
}
