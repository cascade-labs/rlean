//! `lean-data-providers` — framework traits and types.
//!
//! Defines all data-provider traits, request types, and the local/stacked
//! providers.  Plugin implementations (thetadata, massive, etc.) live in
//! `rlean-plugins/` and depend on this crate, not the other way around.
//!
//! # Crate dependency graph (simplified)
//! ```text
//! lean-core
//!   └─ lean-data          (TradeBar, QuoteBar, …)
//!        └─ lean-storage  (ParquetReader/Writer, PathResolver, …)
//!             └─ lean-data-providers   ← this crate (traits + local provider)
//!                  ├─ rlean-plugins/thetadata  (implements IHistoryProvider)
//!                  └─ rlean-plugins/massive    (implements IHistoryProvider)
//! ```

pub mod config;
pub mod custom_data;
pub mod local;
pub mod request;
pub mod stacked;
pub mod traits;

pub use config::ProviderConfig;
pub use custom_data::{ArcCustomDataSource, ICustomDataSource};
pub use local::LocalHistoryProvider;
pub use request::{DataType, DownloadRequest, HistoryRequest};
pub use stacked::{is_not_implemented, StackedHistoryProvider};
pub use traits::{
    IDataDownloader, IFactorFileProvider, IHistoryProvider, ILiveDataProvider, IMapFileProvider,
    IOptionChainProvider,
};

#[cfg(test)]
mod tests;
