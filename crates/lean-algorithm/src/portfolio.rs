use chrono::{TimeZone, Timelike, Utc};
use lean_core::{
    DateTime, Market, NanosecondTimestamp, Price, Quantity, SecurityType, Symbol, SymbolOptionsExt,
};
use lean_data::MarginInterestRate;
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

const HOUR_NANOS: i64 = 3_600_000_000_000;

#[derive(Debug, Clone, Copy, Default)]
struct MarginInterestState {
    next_application: Option<DateTime>,
}

fn next_hourly_funding_time(current_time: DateTime) -> DateTime {
    NanosecondTimestamp(current_time.0.div_euclid(HOUR_NANOS) * HOUR_NANOS + HOUR_NANOS)
}

fn next_eight_hour_funding_time(current_time: DateTime) -> DateTime {
    let dt = current_time.to_utc();
    let date = dt.date_naive();
    let next = if dt.hour() >= 16 {
        date.succ_opt()
            .expect("funding date overflow")
            .and_hms_opt(0, 0, 0)
            .expect("valid midnight")
    } else if dt.hour() >= 8 {
        date.and_hms_opt(16, 0, 0).expect("valid funding time")
    } else {
        date.and_hms_opt(8, 0, 0).expect("valid funding time")
    };
    Utc.from_utc_datetime(&next).into()
}

fn next_funding_time(symbol: &Symbol, current_time: DateTime) -> DateTime {
    if symbol.security_type() == SecurityType::CryptoFuture
        && symbol.market().as_str() == Market::HYPERLIQUID
    {
        next_hourly_funding_time(current_time)
    } else {
        next_eight_hour_funding_time(current_time)
    }
}

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
    pub total_funding: Price,
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
            total_funding: dec!(0),
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

    pub fn portfolio_value_contribution(&self) -> Price {
        match self.symbol.security_type() {
            SecurityType::CryptoFuture => self.unrealized_pnl,
            _ => self.market_value(),
        }
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
    margin_interest_states: RwLock<HashMap<u64, MarginInterestState>>,
    pub total_fees: RwLock<Price>,
    pub total_funding: RwLock<Price>,
}

impl SecurityPortfolioManager {
    pub fn new(starting_cash: Price) -> Self {
        SecurityPortfolioManager {
            cash: RwLock::new(starting_cash),
            starting_cash,
            holdings: RwLock::new(HashMap::new()),
            margin_interest_states: RwLock::new(HashMap::new()),
            total_fees: RwLock::new(dec!(0)),
            total_funding: RwLock::new(dec!(0)),
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
        self.holdings.read().get(&sid).cloned().unwrap_or_else(|| {
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
            .map(|h| h.portfolio_value_contribution())
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

    /// Sum of absolute holdings notional, matching LEAN's
    /// `SecurityPortfolioManager.TotalHoldingsValue` (sum of
    /// `AbsoluteHoldingsValue`). This is the gross market value of all
    /// positions — distinct from `total_portfolio_value`, which for margin
    /// instruments (CryptoFuture/CFD) contributes only unrealized PnL.
    pub fn total_holdings_value(&self) -> Price {
        self.holdings
            .read()
            .values()
            .map(|h| h.market_value().abs())
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

        let realized_pnl_before = h.realized_pnl;
        h.apply_fill(fill_price, fill_quantity, fee);

        let cash_delta = match symbol.security_type() {
            SecurityType::CryptoFuture => (h.realized_pnl - realized_pnl_before) - fee,
            _ => -(fill_price * fill_quantity * contract_multiplier) - fee,
        };
        *self.cash.write() += cash_delta;
        *self.total_fees.write() += fee;
    }

    pub fn apply_margin_interest_rate(&self, rate: &MarginInterestRate) -> Option<Price> {
        let mut holdings = self.holdings.write();
        let holding = holdings.get_mut(&rate.symbol.id.sid)?;
        if !holding.is_invested() || holding.last_price.is_zero() {
            self.margin_interest_states
                .write()
                .remove(&rate.symbol.id.sid);
            return None;
        }

        let mut states = self.margin_interest_states.write();
        let state = states.entry(rate.symbol.id.sid).or_default();
        let mut next_application = match state.next_application {
            Some(time) => time,
            None => {
                let time = next_funding_time(&rate.symbol, rate.time);
                state.next_application = Some(time);
                time
            }
        };

        if rate.time < next_application {
            return None;
        }

        let position_value = holding.market_value();
        let mut funding_cash_delta = Decimal::ZERO;
        while rate.time >= next_application {
            funding_cash_delta -= rate.interest_rate * position_value;
            next_application = next_funding_time(&rate.symbol, next_application);
        }
        state.next_application = Some(next_application);

        holding.total_funding += funding_cash_delta;
        *self.cash.write() += funding_cash_delta;
        *self.total_funding.write() += funding_cash_delta;
        Some(funding_cash_delta)
    }

    pub fn apply_margin_interest_rates<'a>(
        &self,
        rates: impl IntoIterator<Item = &'a MarginInterestRate>,
    ) -> Price {
        rates
            .into_iter()
            .filter_map(|rate| self.apply_margin_interest_rate(rate))
            .sum()
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

    /// Apply C# LEAN split semantics to an equity holding in raw/live mode.
    ///
    /// `split_factor` is the LEAN price factor: 0.5 for a 2:1 forward split,
    /// 10 for a 1:10 reverse split. Holdings are rounded toward zero and the
    /// fractional remainder is paid as cash-in-lieu at the split-adjusted price.
    pub fn apply_split(
        &self,
        symbol: &Symbol,
        split_factor: Price,
        reference_price: Price,
        current_price: Option<Price>,
    ) {
        if split_factor.is_zero() {
            return;
        }

        let mut holdings = self.holdings.write();
        let Some(h) = holdings.get_mut(&symbol.id.sid) else {
            return;
        };
        if !h.is_invested() {
            return;
        }

        let new_quantity = h.quantity / split_factor;
        let whole_quantity = new_quantity.trunc();
        let left_over = new_quantity - whole_quantity;
        let new_average_price = h.average_price * split_factor;
        let split_adjusted_price = current_price
            .filter(|price| !price.is_zero())
            .unwrap_or(reference_price * split_factor);

        h.quantity = whole_quantity;
        h.average_price = new_average_price;
        h.update_price(split_adjusted_price);

        drop(holdings);
        if !left_over.is_zero() {
            *self.cash.write() += left_over * split_adjusted_price;
        }
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
