use lean_core::{Price, Quantity, Symbol, SymbolOptionsExt};
use lean_orders::Order;
use parking_lot::RwLock;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn signum(d: Decimal) -> Decimal {
    if d > dec!(0) {
        dec!(1)
    } else if d < dec!(0) {
        dec!(-1)
    } else {
        dec!(0)
    }
}
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Tracks position in a single security.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityHolding {
    pub symbol: Symbol,
    pub contract_multiplier: Decimal,
    pub quantity: Quantity,
    pub average_price: Price,
    pub unrealized_pnl: Price,
    pub realized_pnl: Price,
    pub total_fees: Price,
    pub last_price: Price,
}

impl SecurityHolding {
    pub fn new(symbol: Symbol) -> Self {
        let contract_multiplier = Self::infer_contract_multiplier(&symbol);
        Self::new_with_multiplier(symbol, contract_multiplier)
    }

    pub fn new_with_multiplier(symbol: Symbol, contract_multiplier: Decimal) -> Self {
        SecurityHolding {
            symbol,
            contract_multiplier,
            quantity: dec!(0),
            average_price: dec!(0),
            unrealized_pnl: dec!(0),
            realized_pnl: dec!(0),
            total_fees: dec!(0),
            last_price: dec!(0),
        }
    }

    pub fn infer_contract_multiplier(symbol: &Symbol) -> Decimal {
        if symbol.option_symbol_id().is_some() {
            dec!(100)
        } else {
            dec!(1)
        }
    }

    pub fn set_contract_multiplier(&mut self, contract_multiplier: Decimal) {
        self.contract_multiplier = contract_multiplier;
        self.update_price(self.last_price);
    }

    pub fn is_long(&self) -> bool {
        self.quantity > dec!(0)
    }
    pub fn is_short(&self) -> bool {
        self.quantity < dec!(0)
    }
    pub fn is_invested(&self) -> bool {
        self.quantity != dec!(0)
    }
    pub fn abs_quantity(&self) -> Quantity {
        self.quantity.abs()
    }

    pub fn get_quantity_value(&self, quantity: Quantity, price: Price) -> Price {
        quantity * price * self.contract_multiplier
    }

    pub fn market_value(&self) -> Price {
        self.get_quantity_value(self.quantity, self.last_price)
    }

    pub fn update_price(&mut self, price: Price) {
        self.last_price = price;
        self.unrealized_pnl =
            (price - self.average_price) * self.quantity * self.contract_multiplier;
    }

    /// Apply a fill to this position.
    pub fn apply_fill(&mut self, fill_price: Price, fill_quantity: Quantity, fee: Price) {
        self.total_fees += fee;
        let current_qty = self.quantity;
        let new_qty = current_qty + fill_quantity;

        if current_qty == dec!(0) {
            // Opening new position
            self.average_price = fill_price;
        } else if signum(current_qty) == signum(fill_quantity) {
            // Adding to existing position — update VWAP
            self.average_price =
                (self.average_price * current_qty + fill_price * fill_quantity) / new_qty;
        } else {
            // Reducing or reversing position
            let qty_closed = current_qty.abs().min(fill_quantity.abs());
            let pnl = (fill_price - self.average_price)
                * qty_closed
                * signum(current_qty)
                * self.contract_multiplier;
            self.realized_pnl += pnl;

            if new_qty == dec!(0) {
                self.average_price = dec!(0);
            } else if signum(new_qty) != signum(current_qty) {
                // Reversal — new average is fill price
                self.average_price = fill_price;
            }
            // Otherwise keep average_price (partial reduction)
        }

        self.quantity = new_qty;
        self.update_price(fill_price);
    }
}

/// Manages the complete portfolio: cash + all holdings.
#[derive(Debug)]
pub struct SecurityPortfolioManager {
    pub cash: RwLock<Price>,
    pub starting_cash: Price,
    holdings: RwLock<HashMap<u64, SecurityHolding>>,
    pub total_fees: RwLock<Price>,
}

impl SecurityPortfolioManager {
    pub fn new(starting_cash: Price) -> Self {
        SecurityPortfolioManager {
            cash: RwLock::new(starting_cash),
            starting_cash,
            holdings: RwLock::new(HashMap::new()),
            total_fees: RwLock::new(dec!(0)),
        }
    }

    pub fn get_holding(&self, symbol: &Symbol) -> SecurityHolding {
        self.holdings
            .read()
            .get(&symbol.id.sid)
            .cloned()
            .unwrap_or_else(|| SecurityHolding::new(symbol.clone()))
    }

    pub fn get_holding_by_sid(&self, sid: u64) -> SecurityHolding {
        self.holdings
            .read()
            .get(&sid)
            .cloned()
            .unwrap_or_else(|| {
                let dummy = lean_core::Symbol::create_equity("UNKNOWN", &lean_core::Market::usa());
                SecurityHolding::new(dummy)
            })
    }

    pub fn is_invested(&self, symbol: &Symbol) -> bool {
        self.holdings
            .read()
            .get(&symbol.id.sid)
            .map(|h| h.is_invested())
            .unwrap_or(false)
    }

    pub fn total_portfolio_value(&self) -> Price {
        let cash = *self.cash.read();
        let holdings_value: Price = self
            .holdings
            .read()
            .values()
            .map(|h| h.market_value())
            .sum();
        cash + holdings_value
    }

    pub fn unrealized_profit(&self) -> Price {
        self.holdings
            .read()
            .values()
            .map(|h| h.unrealized_pnl)
            .sum()
    }

    pub fn total_holdings_value(&self) -> Price {
        self.holdings
            .read()
            .values()
            .map(|h| h.market_value())
            .sum()
    }

    /// Apply an option exercise or assignment directly at the given price.
    ///
    /// Unlike `apply_fill` this does NOT go through the order queue — it
    /// settles the stock leg of an option exercise immediately on the
    /// expiration day so the equity curve stays correct.
    ///
    /// - `fill_quantity` > 0 → buying shares (put assignment, call exercise)
    /// - `fill_quantity` < 0 → selling shares (call assignment, put exercise)
    /// - `fill_price` is the option strike price (the contractual settlement price)
    pub fn apply_exercise(&self, symbol: &Symbol, fill_price: Price, fill_quantity: Quantity) {
        self.apply_exercise_with_market_price(symbol, fill_price, fill_quantity, fill_price);
    }

    pub fn apply_exercise_with_market_price(
        &self,
        symbol: &Symbol,
        fill_price: Price,
        fill_quantity: Quantity,
        market_price: Price,
    ) {
        let mut holdings = self.holdings.write();
        let h = holdings
            .entry(symbol.id.sid)
            .or_insert_with(|| SecurityHolding::new(symbol.clone()));
        h.apply_fill(fill_price, fill_quantity, dec!(0));
        h.update_price(market_price);
        let cash_delta = -(fill_price * fill_quantity);
        *self.cash.write() += cash_delta;
    }

    pub fn apply_fill(
        &self,
        order: &Order,
        fill_price: Price,
        fill_quantity: Quantity,
        fee: Price,
    ) {
        self.apply_fill_with_multiplier(
            &order.symbol,
            fill_price,
            fill_quantity,
            fee,
            SecurityHolding::infer_contract_multiplier(&order.symbol),
        );
    }

    pub fn apply_fill_with_multiplier(
        &self,
        symbol: &Symbol,
        fill_price: Price,
        fill_quantity: Quantity,
        fee: Price,
        contract_multiplier: Decimal,
    ) {
        let mut holdings = self.holdings.write();
        let h = holdings.entry(symbol.id.sid).or_insert_with(|| {
            SecurityHolding::new_with_multiplier(symbol.clone(), contract_multiplier)
        });
        if h.contract_multiplier != contract_multiplier {
            h.set_contract_multiplier(contract_multiplier);
        }

        h.apply_fill(fill_price, fill_quantity, fee);

        // Update cash
        let cash_delta = -(fill_price * fill_quantity * contract_multiplier) - fee;
        *self.cash.write() += cash_delta;
        *self.total_fees.write() += fee;
    }

    pub fn settle_fill_without_cash(
        &self,
        symbol: &Symbol,
        fill_price: Price,
        fill_quantity: Quantity,
        contract_multiplier: Decimal,
    ) {
        let mut holdings = self.holdings.write();
        let h = holdings.entry(symbol.id.sid).or_insert_with(|| {
            SecurityHolding::new_with_multiplier(symbol.clone(), contract_multiplier)
        });
        if h.contract_multiplier != contract_multiplier {
            h.set_contract_multiplier(contract_multiplier);
        }
        h.apply_fill(fill_price, fill_quantity, dec!(0));
    }

    pub fn set_holdings(
        &self,
        symbol: &Symbol,
        average_price: Price,
        quantity: Quantity,
        contract_multiplier: Decimal,
    ) {
        let mut holdings = self.holdings.write();
        let h = holdings.entry(symbol.id.sid).or_insert_with(|| {
            SecurityHolding::new_with_multiplier(symbol.clone(), contract_multiplier)
        });
        h.symbol = symbol.clone();
        h.quantity = quantity;
        h.average_price = average_price;
        h.set_contract_multiplier(contract_multiplier);
        h.update_price(average_price);
    }

    pub fn update_prices(&self, symbol: &Symbol, price: Price) {
        if let Some(h) = self.holdings.write().get_mut(&symbol.id.sid) {
            h.update_price(price);
        }
    }

    pub fn all_holdings(&self) -> Vec<SecurityHolding> {
        self.holdings.read().values().cloned().collect()
    }

    pub fn invested_symbols(&self) -> Vec<Symbol> {
        self.holdings
            .read()
            .values()
            .filter(|h| h.is_invested())
            .map(|h| h.symbol.clone())
            .collect()
    }

    /// Percentage return from starting portfolio value.
    pub fn total_return_pct(&self) -> Price {
        if self.starting_cash.is_zero() {
            return dec!(0);
        }
        (self.total_portfolio_value() - self.starting_cash) / self.starting_cash
    }
}
