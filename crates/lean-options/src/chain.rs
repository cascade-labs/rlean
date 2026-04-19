use crate::contract::OptionContract;
use crate::filter_universe::OptionFilterUniverse;
use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;

/// Collection of option contracts for a single underlying at a point in time.
#[derive(Debug, Clone)]
pub struct OptionChain {
    /// The canonical option symbol (e.g. ?SPY)
    pub canonical_symbol: Symbol,
    /// Current price of the underlying
    pub underlying_price: Decimal,
    /// All contracts keyed by their full symbol
    pub contracts: HashMap<Symbol, OptionContract>,
}

impl OptionChain {
    pub fn new(canonical_symbol: Symbol, underlying_price: Decimal) -> Self {
        OptionChain {
            canonical_symbol,
            underlying_price,
            contracts: HashMap::new(),
        }
    }

    pub fn add_contract(&mut self, contract: OptionContract) {
        self.contracts.insert(contract.symbol.clone(), contract);
    }

    /// Returns contracts filtered by the given function.
    pub fn filter<F: Fn(&OptionContract) -> bool>(&self, f: F) -> Vec<&OptionContract> {
        self.contracts.values().filter(|c| f(c)).collect()
    }

    /// Returns a filter universe for fluent-style filtering.
    pub fn filter_universe(&self) -> OptionFilterUniverse {
        OptionFilterUniverse::new(
            self.contracts.values().cloned().collect(),
            self.underlying_price,
        )
    }

    /// All contracts sorted by expiry, then strike.
    pub fn sorted(&self) -> Vec<&OptionContract> {
        let mut v: Vec<&OptionContract> = self.contracts.values().collect();
        v.sort_by(|a, b| a.expiry.cmp(&b.expiry).then(a.strike.cmp(&b.strike)));
        v
    }
}
