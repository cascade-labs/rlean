use lean_core::exchange_hours::ExchangeHours;
use lean_core::{Price, Resolution, Symbol, SymbolProperties};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// A single tradeable security in the algorithm's universe.
#[derive(Debug)]
pub struct Security {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub symbol_properties: SymbolProperties,
    pub exchange_hours: ExchangeHours,
    pub leverage: f64,
    pub is_tradable: bool,
    pub is_delisted: bool,
    pub price: RwLock<Price>,
}

impl Security {
    pub fn new(
        symbol: Symbol,
        resolution: Resolution,
        symbol_properties: SymbolProperties,
        exchange_hours: ExchangeHours,
    ) -> Self {
        Security {
            symbol,
            resolution,
            symbol_properties,
            exchange_hours,
            leverage: 1.0,
            is_tradable: true,
            is_delisted: false,
            price: RwLock::new(rust_decimal_macros::dec!(0)),
        }
    }

    pub fn current_price(&self) -> Price {
        *self.price.read()
    }

    pub fn set_price(&self, price: Price) {
        *self.price.write() = price;
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
}
