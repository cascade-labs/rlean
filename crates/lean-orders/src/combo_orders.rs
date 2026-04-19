use lean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::order::{Order, OrderType};

// ─── ComboLegDetails ─────────────────────────────────────────────────────────

/// Describes one leg of a combo (multi-leg) order group.
///
/// In C#, leg details are managed via `GroupOrderManager`; here we flatten them
/// into a plain struct that can be embedded directly in the combo order structs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboLegDetails {
    /// The security for this leg.
    pub symbol: Symbol,
    /// The signed quantity ratio for this leg relative to the group quantity.
    /// Positive = buy, negative = sell.
    pub quantity: Quantity,
    /// The order ID generated for this leg.
    pub order_id: i64,
}

impl ComboLegDetails {
    pub fn new(symbol: Symbol, quantity: Quantity, order_id: i64) -> Self {
        Self {
            symbol,
            quantity,
            order_id,
        }
    }
}

// ─── ComboMarketOrder ────────────────────────────────────────────────────────

/// A combo market order — all legs execute simultaneously at the prevailing
/// market prices. Commonly used for options spreads and pairs trades.
///
/// Mirrors C# `ComboMarketOrder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboMarketOrder {
    /// The primary (first-leg) order record. Additional legs are in `legs`.
    pub order: Order,
    /// All legs in this combo group, including the primary leg.
    pub legs: Vec<ComboLegDetails>,
}

impl ComboMarketOrder {
    /// Create a new combo market order.
    ///
    /// `symbol` and `quantity` refer to the primary leg; additional legs are
    /// supplied via `legs`. The primary leg should also be included in `legs`
    /// for uniform processing.
    pub fn new(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        time: DateTime,
        tag: &str,
        legs: Vec<ComboLegDetails>,
    ) -> Self {
        let mut order = Order::market(id, symbol, quantity, time, tag);
        order.order_type = OrderType::ComboMarket;
        Self { order, legs }
    }

    /// Returns the number of legs in this combo.
    pub fn leg_count(&self) -> usize {
        self.legs.len()
    }

    /// Looks up a leg by order ID.
    pub fn find_leg(&self, order_id: i64) -> Option<&ComboLegDetails> {
        self.legs.iter().find(|l| l.order_id == order_id)
    }
}

// ─── ComboLimitOrder ─────────────────────────────────────────────────────────

/// A combo limit order — all legs execute as a unit only when the net debit/
/// credit of the combo meets the specified `limit_price`.
///
/// `limit_price` is the *net* price of the whole combo (e.g., the net debit
/// for a spread), shared across all legs — mirroring how `GroupOrderManager.LimitPrice`
/// works in C# `ComboLimitOrder`.
///
/// Mirrors C# `ComboLimitOrder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboLimitOrder {
    /// The primary (first-leg) order record.
    pub order: Order,
    /// The net limit price for the entire combo.
    pub limit_price: Price,
    /// All legs in this combo group.
    pub legs: Vec<ComboLegDetails>,
}

impl ComboLimitOrder {
    /// Create a new combo limit order.
    pub fn new(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        limit_price: Price,
        time: DateTime,
        tag: &str,
        legs: Vec<ComboLegDetails>,
    ) -> Self {
        let mut order = Order::market(id, symbol, quantity, time, tag);
        order.order_type = OrderType::ComboLimit;
        order.limit_price = Some(limit_price);
        Self {
            order,
            limit_price,
            legs,
        }
    }

    /// Returns `true` if the given net market price satisfies the limit condition.
    ///
    /// For a debit combo (positive quantity), fills when net price <= limit_price.
    /// For a credit combo (negative quantity), fills when net price >= limit_price.
    pub fn would_fill(&self, net_market_price: Price) -> bool {
        if self.order.quantity > Decimal::ZERO {
            net_market_price <= self.limit_price
        } else {
            net_market_price >= self.limit_price
        }
    }

    /// Looks up a leg by order ID.
    pub fn find_leg(&self, order_id: i64) -> Option<&ComboLegDetails> {
        self.legs.iter().find(|l| l.order_id == order_id)
    }
}

// ─── ComboLegLimitOrder ──────────────────────────────────────────────────────

/// A combo order variant where each leg has its *own* per-leg limit price,
/// as opposed to `ComboLimitOrder` which uses a single net combo price.
///
/// Mirrors C# `ComboLegLimitOrder`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComboLegLimitOrder {
    /// The primary (first-leg) order record.
    pub order: Order,
    /// The per-leg limit price for this specific leg.
    pub limit_price: Price,
    /// All legs in this combo group.
    pub legs: Vec<ComboLegDetails>,
}

impl ComboLegLimitOrder {
    /// Create a new combo leg limit order (per-leg pricing).
    pub fn new(
        id: i64,
        symbol: Symbol,
        quantity: Quantity,
        limit_price: Price,
        time: DateTime,
        tag: &str,
        legs: Vec<ComboLegDetails>,
    ) -> Self {
        let mut order = Order::market(id, symbol, quantity, time, tag);
        order.order_type = OrderType::ComboLegLimit;
        order.limit_price = Some(limit_price);
        Self {
            order,
            limit_price,
            legs,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{DateTime, Market, Symbol};
    use rust_decimal_macros::dec;

    fn sym(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    #[test]
    fn combo_market_find_leg() {
        let legs = vec![
            ComboLegDetails::new(sym("SPY"), dec!(1), 1),
            ComboLegDetails::new(sym("QQQ"), dec!(-1), 2),
        ];
        let order = ComboMarketOrder::new(1, sym("SPY"), dec!(1), DateTime::EPOCH, "", legs);
        assert!(order.find_leg(2).is_some());
        assert!(order.find_leg(99).is_none());
    }

    #[test]
    fn combo_limit_would_fill_debit() {
        let order =
            ComboLimitOrder::new(1, sym("SPY"), dec!(1), dec!(5), DateTime::EPOCH, "", vec![]);
        assert!(order.would_fill(dec!(4))); // cheaper than limit
        assert!(order.would_fill(dec!(5))); // at limit
        assert!(!order.would_fill(dec!(6))); // too expensive
    }

    #[test]
    fn combo_limit_would_fill_credit() {
        // Negative qty = credit spread (we want more credit)
        let order = ComboLimitOrder::new(
            1,
            sym("SPY"),
            dec!(-1),
            dec!(3),
            DateTime::EPOCH,
            "",
            vec![],
        );
        assert!(order.would_fill(dec!(4))); // more credit than limit
        assert!(order.would_fill(dec!(3))); // at limit
        assert!(!order.would_fill(dec!(2))); // less credit
    }
}
