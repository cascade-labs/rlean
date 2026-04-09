use lean_core::{DateTime, Resolution, Symbol};

#[derive(Debug, Clone)]
pub struct UniverseSettings {
    pub resolution: Resolution,
    pub fill_data_forward: bool,
    pub extended_market_hours: bool,
    pub minimum_time_in_universe: lean_core::TimeSpan,
    pub leverage: f64,
}

impl Default for UniverseSettings {
    fn default() -> Self {
        UniverseSettings {
            resolution: Resolution::Daily,
            fill_data_forward: true,
            extended_market_hours: false,
            minimum_time_in_universe: lean_core::TimeSpan::ONE_DAY,
            leverage: 1.0,
        }
    }
}

pub trait Universe: Send + Sync {
    fn select_symbols(&self, utc_time: DateTime, data: &[lean_core::Symbol]) -> Vec<Symbol>;
    fn settings(&self) -> &UniverseSettings;
}

pub trait UniverseSelectionModel: Send + Sync {
    fn create_universes(&self) -> Vec<Box<dyn Universe>>;
}

/// Manual universe — fixed list of symbols.
pub struct ManualUniverse {
    pub symbols: Vec<Symbol>,
    pub settings: UniverseSettings,
}

impl ManualUniverse {
    pub fn new(symbols: Vec<Symbol>, settings: UniverseSettings) -> Self {
        ManualUniverse { symbols, settings }
    }
}

impl Universe for ManualUniverse {
    fn select_symbols(&self, _utc_time: DateTime, _data: &[Symbol]) -> Vec<Symbol> {
        self.symbols.clone()
    }

    fn settings(&self) -> &UniverseSettings { &self.settings }
}
