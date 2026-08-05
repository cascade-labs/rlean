//! Native data-provider contracts.
//!
//! The layering follows C# LEAN's `IHistoryProvider`/`IDataQueueHandler`
//! split: history is a finite pull, live data is a subscription whose values
//! are pushed by the provider. Persistence is an independent concern wrapped
//! around history providers, not implemented inside a vendor adapter.

mod history;
mod live;
mod massive;
mod massive_live;
mod routed_live;
mod thetadata;
mod tradier;
mod verglas_store;

pub use history::{
    CacheFirstHistoryProvider, Coverage, HistoricalData, HistoricalDataProvider,
    HistoricalDataStore, HistoryRequest, TimeRange,
};
pub use live::{LiveDataEvent, LiveDataProvider, LiveSubscription};
pub use massive::{MassiveConfig, MassiveHistoricalDataProvider};
pub use massive_live::{MassiveLiveConfig, MassiveLiveDataProvider};
pub use routed_live::{RoutedLiveDataProvider, VerglasCustomLiveDataProvider};
pub use thetadata::{ThetaDataConfig, ThetaDataHistoricalDataProvider};
pub use tradier::{TradierEnvironment, TradierLiveDataProvider, TradierMarketDataConfig};
pub use verglas_store::VerglasHistoricalDataStore;
