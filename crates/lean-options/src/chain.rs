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

    /// Update an existing chain without replacing the whole contract map.
    ///
    /// This keeps Python/engine chain views stable on high-frequency data where
    /// the universe shape is mostly unchanged and only quote/trade fields move.
    pub fn update_from(&mut self, next: &OptionChain) {
        self.canonical_symbol = next.canonical_symbol.clone();
        self.underlying_price = next.underlying_price;

        self.contracts
            .retain(|symbol, _| next.contracts.contains_key(symbol));

        for (symbol, next_contract) in &next.contracts {
            if let Some(current_contract) = self.contracts.get_mut(symbol) {
                current_contract.strike = next_contract.strike;
                current_contract.expiry = next_contract.expiry;
                current_contract.right = next_contract.right;
                current_contract.style = next_contract.style;
                current_contract.data = next_contract.data.clone();
                current_contract.contract_unit_of_trade = next_contract.contract_unit_of_trade;
                current_contract.contract_multiplier = next_contract.contract_multiplier;
            } else {
                self.contracts.insert(symbol.clone(), next_contract.clone());
            }
        }
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

    /// All contracts in a canonical, deterministic order: expiry, then strike,
    /// then right (Call before Put).
    ///
    /// `contracts` is a `HashMap`, whose `.values()` iteration order is randomized
    /// per process. Any consumer that exposes contracts in iteration order (e.g.
    /// the Python chain accessors) MUST go through this so backtests are
    /// reproducible — otherwise a strategy that stably tie-breaks at the money
    /// (call vs put at the same strike) picks different contracts each run.
    pub fn sorted(&self) -> Vec<&OptionContract> {
        let mut v: Vec<&OptionContract> = self.contracts.values().collect();
        v.sort_by(|a, b| {
            a.expiry
                .cmp(&b.expiry)
                .then(a.strike.cmp(&b.strike))
                .then(a.right.cmp(&b.right))
        });
        v
    }
}
