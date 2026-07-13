use crate::buying_power::BuyingPowerModel;
use crate::portfolio::{SecurityHolding, SharedHoldings};
use parking_lot::RwLock;
use rlean_core::exchange_hours::ExchangeHours;
use rlean_core::{Price, Resolution, SecurityType, Symbol, SymbolProperties};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;

/// A single tradeable security in the algorithm's universe.
#[derive(Debug)]
pub struct Security {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub symbol_properties: SymbolProperties,
    pub exchange_hours: Arc<ExchangeHours>,
    pub leverage: RwLock<f64>,
    pub buying_power_model: RwLock<BuyingPowerModel>,
    pub is_tradable: bool,
    pub is_delisted: bool,
    pub price: RwLock<Price>,
    pub bid_price: RwLock<Price>,
    pub ask_price: RwLock<Price>,
    /// `lot_size` as a `Decimal`, converted once at construction. `symbol_properties`
    /// is immutable, so this never changes over the security's lifetime. Cached to
    /// avoid re-running `Decimal::from_f64` on every framework slice.
    lot_size_decimal: Decimal,
    /// `minimum_price_variation` as a `Decimal`, cached at construction for the same
    /// reason as `lot_size_decimal`.
    minimum_price_variation_decimal: Decimal,
    holdings: SharedHoldings,
}

impl Security {
    pub fn new(
        symbol: Symbol,
        resolution: Resolution,
        symbol_properties: SymbolProperties,
        exchange_hours: Arc<ExchangeHours>,
        holdings: SharedHoldings,
    ) -> Self {
        // Base securities back custom-data subscriptions (e.g. flow alerts, FRED
        // series). They are never tradable positions, so they must not seed an
        // entry in the shared holdings map — otherwise they surface as zero-
        // quantity "holdings" in portfolio snapshots. Genuine positions insert
        // their holding lazily on fill/set_holdings.
        if symbol.security_type() != SecurityType::Base {
            holdings
                .write()
                .entry(symbol.id.sid)
                .or_insert_with(|| SecurityHolding::new(symbol.clone()));
        }
        let lot_size_decimal = Decimal::from_f64(symbol_properties.lot_size)
            .filter(|lot| *lot > Decimal::ZERO)
            .unwrap_or(Decimal::ONE);
        let minimum_price_variation_decimal =
            Decimal::from_f64(symbol_properties.minimum_price_variation)
                .filter(|tick| *tick > Decimal::ZERO)
                .unwrap_or(Decimal::new(1, 2));
        Security {
            symbol,
            resolution,
            symbol_properties,
            exchange_hours,
            leverage: RwLock::new(1.0),
            buying_power_model: RwLock::new(BuyingPowerModel::SecurityMargin),
            is_tradable: true,
            is_delisted: false,
            price: RwLock::new(rust_decimal_macros::dec!(0)),
            bid_price: RwLock::new(rust_decimal_macros::dec!(0)),
            ask_price: RwLock::new(rust_decimal_macros::dec!(0)),
            lot_size_decimal,
            minimum_price_variation_decimal,
            holdings,
        }
    }

    /// `lot_size` converted to `Decimal`, cached at construction. Matches the
    /// framework's historical inline conversion: falls back to `Decimal::ONE` when
    /// the configured lot size is non-positive or not representable.
    pub fn lot_size_decimal(&self) -> Decimal {
        self.lot_size_decimal
    }

    /// `minimum_price_variation` converted to `Decimal`, cached at construction.
    /// Falls back to `0.01` when the configured tick is non-positive or not
    /// representable, matching the framework's historical inline conversion.
    pub fn minimum_price_variation_decimal(&self) -> Decimal {
        self.minimum_price_variation_decimal
    }

    pub fn current_price(&self) -> Price {
        *self.price.read()
    }

    pub fn set_price(&self, price: Price) {
        *self.price.write() = price;
    }

    pub fn bid_price(&self) -> Price {
        *self.bid_price.read()
    }

    pub fn ask_price(&self) -> Price {
        *self.ask_price.read()
    }

    pub fn set_quote(&self, bid_price: Price, ask_price: Price) {
        *self.bid_price.write() = bid_price;
        *self.ask_price.write() = ask_price;
        if bid_price > rust_decimal_macros::dec!(0) && ask_price > rust_decimal_macros::dec!(0) {
            *self.price.write() = (bid_price + ask_price) / rust_decimal_macros::dec!(2);
        }
    }

    pub fn holding(&self) -> SecurityHolding {
        self.holdings
            .read()
            .get(&self.symbol.id.sid)
            .cloned()
            .unwrap_or_else(|| SecurityHolding::new(self.symbol.clone()))
    }

    pub fn leverage(&self) -> f64 {
        *self.leverage.read()
    }

    pub fn set_leverage(&self, leverage: f64) {
        BuyingPowerModel::validate_leverage(leverage);
        *self.leverage.write() = leverage;
    }

    pub fn buying_power_model(&self) -> BuyingPowerModel {
        *self.buying_power_model.read()
    }

    pub fn set_buying_power_model(&self, model: BuyingPowerModel) {
        *self.buying_power_model.write() = model;
    }
}

/// All securities currently in the algorithm.
#[derive(Debug, Default)]
pub struct SecurityManager {
    securities: HashMap<u64, Arc<Security>>,
}

impl SecurityManager {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn add(&mut self, security: Security) -> Arc<Security> {
        let sid = security.symbol.id.sid;
        let s = Arc::new(security);
        self.securities.insert(sid, s.clone());
        s
    }

    pub fn get(&self, symbol: &Symbol) -> Option<Arc<Security>> {
        self.securities.get(&symbol.id.sid).cloned()
    }

    pub fn contains(&self, symbol: &Symbol) -> bool {
        self.securities.contains_key(&symbol.id.sid)
    }

    pub fn remove(&mut self, symbol: &Symbol) -> Option<Arc<Security>> {
        self.securities.remove(&symbol.id.sid)
    }

    pub fn all(&self) -> impl Iterator<Item = &Arc<Security>> {
        self.securities.values()
    }

    pub fn count(&self) -> usize {
        self.securities.len()
    }

    pub fn update_price(&self, symbol: &Symbol, price: Price) {
        if let Some(sec) = self.securities.get(&symbol.id.sid) {
            sec.set_price(price);
        }
    }

    pub fn update_quote(&self, symbol: &Symbol, bid_price: Price, ask_price: Price) {
        if let Some(sec) = self.securities.get(&symbol.id.sid) {
            sec.set_quote(bid_price, ask_price);
        }
    }
}
