use lean_core::{DateTime, Resolution, Symbol};

/// Type of market data to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    TradeBar,
    QuoteBar,
    Tick,
    OpenInterest,
    /// Request a provider to generate/cache a factor file for the symbol.
    /// Returns `Ok(vec![])` on success (the file is written as a side-effect).
    /// Providers that do not support corporate actions return `NotImplemented:`.
    FactorFile,
    /// Request a provider to generate/cache a map file for the symbol.
    /// Returns `Ok(vec![])` on success (the file is written as a side-effect).
    /// Providers that do not support ticker details return `NotImplemented:`.
    MapFile,
}

/// A request for historical data — mirrors C# `HistoryRequest`.
#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub start: DateTime,
    pub end: DateTime,
    pub data_type: DataType,
}

/// A multi-symbol history request for providers that can fetch/cache symbols in
/// batches. The framework groups subscriptions by date/resolution/tick type and
/// can call this without changing provider-specific request semantics.
#[derive(Debug, Clone)]
pub struct HistoryBatchRequest {
    pub symbols: Vec<Symbol>,
    pub resolution: Resolution,
    pub start: DateTime,
    pub end: DateTime,
    pub data_type: DataType,
}

#[derive(Debug, Clone, Default)]
pub struct MarketDataBatch {
    pub trade_bars: Vec<lean_data::TradeBar>,
    pub quote_bars: Vec<lean_data::QuoteBar>,
    pub ticks: Vec<lean_data::Tick>,
}

/// A request to download data to the local store — mirrors C# `DataDownloaderGetParameters`.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub start: chrono::NaiveDate,
    pub end: chrono::NaiveDate,
    pub data_root: std::path::PathBuf,
}
