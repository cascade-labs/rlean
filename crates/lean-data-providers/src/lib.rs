//! `lean-data-providers` — Rust equivalent of `QuantConnect.Interfaces`.
//!
//! This crate defines all data-provider traits and request/config types used
//! across the Lean-Rust workspace.  It does **not** depend on any specific
//! provider implementation (polygon, thetadata, etc.); those crates depend
//! on this one and implement the traits.
//!
//! # Crate dependency graph (simplified)
//! ```text
//! lean-core
//!   └─ lean-data          (TradeBar, QuoteBar, …)
//!        └─ lean-storage  (ParquetReader/Writer, PathResolver, …)
//!             └─ lean-data-providers   ← this crate (traits only)
//!                  ├─ lean-polygon     (implements IHistoryProvider)
//!                  └─ lean-thetadata   (implements IHistoryProvider)
//!                       └─ rlean/src/providers.rs  (registry / factory)
//! ```

pub mod config;
pub mod local;
pub mod request;
pub mod stacked;
pub mod traits;
pub mod thetadata_models;
pub mod thetadata_client;

pub use config::ProviderConfig;
pub use local::LocalHistoryProvider;
pub use request::{DataType, DownloadRequest, HistoryRequest};
pub use stacked::{StackedHistoryProvider, is_not_implemented};
pub use traits::{
    IDataDownloader, IFactorFileProvider, IHistoryProvider, ILiveDataProvider,
    IMapFileProvider, IOptionChainProvider,
};
pub use thetadata_client::ThetaDataClient;
pub use thetadata_models::{
    V3OptionEod, V3OptionOhlc, V3OptionQuote, V3OptionTrade, V3IndexPrice,
    QuoteBar, OhlcBar, TradeTick, EodBar, IndexPrice, OpenInterest,
    parse_date, ms_of_day_from_timestamp, normalize_right, normalize_strike,
    normalize_expiration, exchange_name,
};

#[cfg(test)]
mod tests;
