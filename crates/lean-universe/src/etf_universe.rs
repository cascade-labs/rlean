use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;
use serde::{Deserialize, Serialize};

/// Holding information for an ETF constituent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EtfConstituent {
    pub symbol: Symbol,
    pub weight: Decimal,
    pub shares_held: Option<Decimal>,
    pub market_value: Option<Decimal>,
}

/// Universe that selects constituents of an ETF.
/// Mirrors C# ETFConstituentsUniverseSelectionModel.
pub struct EtfConstituentsUniverse {
    pub etf_symbol: Symbol,
    constituents: HashMap<String, EtfConstituent>,
    filter: Option<Box<dyn Fn(&[EtfConstituent]) -> Vec<Symbol> + Send + Sync>>,
}

impl EtfConstituentsUniverse {
    pub fn new(etf_symbol: Symbol) -> Self {
        Self { etf_symbol, constituents: HashMap::new(), filter: None }
    }

    pub fn with_filter(mut self, f: impl Fn(&[EtfConstituent]) -> Vec<Symbol> + Send + Sync + 'static) -> Self {
        self.filter = Some(Box::new(f));
        self
    }

    /// Load constituent data (call this with fetched data)
    pub fn load_constituents(&mut self, constituents: Vec<EtfConstituent>) {
        self.constituents = constituents.into_iter()
            .map(|c| (c.symbol.value.clone(), c))
            .collect();
    }

    /// Select symbols from loaded constituents, optionally applying filter
    pub fn select_symbols(&self) -> Vec<Symbol> {
        let all: Vec<&EtfConstituent> = self.constituents.values().collect();
        if let Some(f) = &self.filter {
            let all_owned: Vec<EtfConstituent> = all.iter().map(|c| (*c).clone()).collect();
            f(&all_owned)
        } else {
            all.iter().map(|c| c.symbol.clone()).collect()
        }
    }

    pub fn constituent_weight(&self, ticker: &str) -> Option<Decimal> {
        self.constituents.get(ticker).map(|c| c.weight)
    }

    pub fn all_constituents(&self) -> Vec<&EtfConstituent> {
        self.constituents.values().collect()
    }
}

/// Well-known ETF constituent universes
pub struct EtfUniverses;

impl EtfUniverses {
    pub fn sp500(etf_symbol: Symbol) -> EtfConstituentsUniverse {
        EtfConstituentsUniverse::new(etf_symbol)
    }
    pub fn nasdaq100(etf_symbol: Symbol) -> EtfConstituentsUniverse {
        EtfConstituentsUniverse::new(etf_symbol)
    }
    pub fn russell2000(etf_symbol: Symbol) -> EtfConstituentsUniverse {
        EtfConstituentsUniverse::new(etf_symbol)
    }
}
