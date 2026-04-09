use lean_core::{DateTime, Resolution, Symbol};

/// Type of market data to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    TradeBar,
    QuoteBar,
    Tick,
    OpenInterest,
}

/// A request for historical data — mirrors C# `HistoryRequest`.
#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub symbol:     Symbol,
    pub resolution: Resolution,
    pub start:      DateTime,
    pub end:        DateTime,
    pub data_type:  DataType,
}

/// A request to download data to the local store — mirrors C# `DataDownloaderGetParameters`.
#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub symbol:     Symbol,
    pub resolution: Resolution,
    pub start:      chrono::NaiveDate,
    pub end:        chrono::NaiveDate,
    pub data_root:  std::path::PathBuf,
}
