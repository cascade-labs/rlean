use lean_core::Symbol;
use crate::CoarseFundamental;

/// Filters the coarse universe to a specific market cap tier
pub struct MarketCapUniverseSelectionModel {
    pub min_market_cap: Option<rust_decimal::Decimal>,
    pub max_market_cap: Option<rust_decimal::Decimal>,
    pub top_n: Option<usize>,
}

impl MarketCapUniverseSelectionModel {
    /// Top N by market cap
    pub fn large_cap(top_n: usize) -> Self {
        Self { min_market_cap: None, max_market_cap: None, top_n: Some(top_n) }
    }

    pub fn mid_cap(min: rust_decimal::Decimal, max: rust_decimal::Decimal) -> Self {
        Self { min_market_cap: Some(min), max_market_cap: Some(max), top_n: None }
    }

    pub fn select(&self, coarse: &[CoarseFundamental]) -> Vec<Symbol> {
        let mut filtered: Vec<&CoarseFundamental> = coarse.iter().filter(|c| {
            if let Some(min) = self.min_market_cap {
                if c.market_cap < min { return false; }
            }
            if let Some(max) = self.max_market_cap {
                if c.market_cap > max { return false; }
            }
            true
        }).collect();
        filtered.sort_by(|a, b| b.market_cap.partial_cmp(&a.market_cap).unwrap_or(std::cmp::Ordering::Equal));
        if let Some(n) = self.top_n { filtered.truncate(n); }
        filtered.iter().map(|c| c.symbol.clone()).collect()
    }
}

/// Sector-filtered universe
pub struct SectorUniverseSelectionModel {
    pub sector_codes: Vec<i32>,
    pub min_dollar_volume: Option<rust_decimal::Decimal>,
}

impl SectorUniverseSelectionModel {
    pub fn new(sector_codes: Vec<i32>) -> Self {
        Self { sector_codes, min_dollar_volume: None }
    }

    pub fn with_min_dollar_volume(mut self, min: rust_decimal::Decimal) -> Self {
        self.min_dollar_volume = Some(min);
        self
    }

    pub fn select(&self, coarse: &[CoarseFundamental]) -> Vec<Symbol> {
        coarse.iter()
            .filter(|c| {
                if let Some(min_dv) = self.min_dollar_volume {
                    if c.dollar_volume < min_dv { return false; }
                }
                true // sector filtering requires fine fundamental data
            })
            .map(|c| c.symbol.clone())
            .collect()
    }
}

/// Liquid universe: top N stocks by dollar volume
pub struct LiquidUniverseSelectionModel {
    pub top_n: usize,
    pub min_price: Option<rust_decimal::Decimal>,
}

impl LiquidUniverseSelectionModel {
    pub fn new(top_n: usize) -> Self {
        Self { top_n, min_price: None }
    }

    pub fn with_min_price(mut self, min: rust_decimal::Decimal) -> Self {
        self.min_price = Some(min);
        self
    }

    pub fn select(&self, coarse: &[CoarseFundamental]) -> Vec<Symbol> {
        let mut filtered: Vec<&CoarseFundamental> = coarse.iter().filter(|c| {
            if let Some(min_p) = self.min_price {
                if c.price < min_p { return false; }
            }
            true
        }).collect();
        filtered.sort_by(|a, b| b.dollar_volume.partial_cmp(&a.dollar_volume).unwrap_or(std::cmp::Ordering::Equal));
        filtered.truncate(self.top_n);
        filtered.iter().map(|c| c.symbol.clone()).collect()
    }
}
