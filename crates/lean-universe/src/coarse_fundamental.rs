use crate::universe::{Universe, UniverseSelectionModel, UniverseSettings};
use lean_core::{DateTime, Price, Symbol};

#[derive(Debug, Clone)]
pub struct CoarseFundamental {
    pub symbol: Symbol,
    pub dollar_volume: Price,
    pub price: Price,
    pub market_cap: Price,
    pub has_fundamental_data: bool,
    pub market: String,
}

pub trait CoarseFilter: Send + Sync {
    fn select(&self, coarse: &[CoarseFundamental]) -> Vec<Symbol>;
}

pub struct CoarseUniverseSelectionModel {
    pub filter: Box<dyn CoarseFilter>,
    pub settings: UniverseSettings,
}

impl CoarseUniverseSelectionModel {
    pub fn new(filter: impl CoarseFilter + 'static) -> Self {
        CoarseUniverseSelectionModel {
            filter: Box::new(filter),
            settings: UniverseSettings::default(),
        }
    }
}

/// Filter function adapter — lets you pass a closure.
pub struct FnCoarseFilter<F: Fn(&[CoarseFundamental]) -> Vec<Symbol> + Send + Sync> {
    func: F,
}

impl<F: Fn(&[CoarseFundamental]) -> Vec<Symbol> + Send + Sync> FnCoarseFilter<F> {
    pub fn new(func: F) -> Self { FnCoarseFilter { func } }
}

impl<F: Fn(&[CoarseFundamental]) -> Vec<Symbol> + Send + Sync> CoarseFilter for FnCoarseFilter<F> {
    fn select(&self, coarse: &[CoarseFundamental]) -> Vec<Symbol> {
        (self.func)(coarse)
    }
}
