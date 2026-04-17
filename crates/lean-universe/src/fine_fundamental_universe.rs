use crate::{fine_fundamental::FineFundamental, CoarseFundamental};
use lean_core::Symbol;

pub type CoarseFilter = dyn Fn(&[CoarseFundamental]) -> Vec<Symbol> + Send + Sync;
pub type FineFilter = dyn Fn(&[FineFundamental]) -> Vec<Symbol> + Send + Sync;

/// Universe that first runs a coarse filter, then passes coarse survivors
/// through a fine fundamental filter. Mirrors C# FineFundamentalUniverseSelectionModel.
pub struct FineFundamentalUniverseSelectionModel {
    /// Coarse filter: given market cap + dollar volume data, return symbols to keep
    pub coarse_filter: Box<CoarseFilter>,
    /// Fine filter: given full fundamental data for coarse survivors, return final symbols
    pub fine_filter: Box<FineFilter>,
}

impl FineFundamentalUniverseSelectionModel {
    pub fn new(
        coarse: impl Fn(&[CoarseFundamental]) -> Vec<Symbol> + Send + Sync + 'static,
        fine: impl Fn(&[FineFundamental]) -> Vec<Symbol> + Send + Sync + 'static,
    ) -> Self {
        Self {
            coarse_filter: Box::new(coarse),
            fine_filter: Box::new(fine),
        }
    }

    /// Select symbols from coarse data using the coarse filter.
    /// In a real system, fine data would then be fetched and filtered.
    pub fn select_coarse(&self, coarse_data: &[CoarseFundamental]) -> Vec<Symbol> {
        (self.coarse_filter)(coarse_data)
    }

    /// Apply fine filter to fine fundamental data for the coarse survivors.
    pub fn select_fine(&self, fine_data: &[FineFundamental]) -> Vec<Symbol> {
        (self.fine_filter)(fine_data)
    }
}
