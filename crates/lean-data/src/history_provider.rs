use std::future::Future;
use std::pin::Pin;
use lean_core::{DateTime, Resolution, Result as LeanResult, Symbol};
use crate::TradeBar;

/// Provides historical market data on demand.
///
/// Implementors fetch and locally cache data so the backtest engine
/// can iterate over dates without pre-staging every file.
pub trait IHistoricalDataProvider: Send + Sync {
    /// Fetch trade bars for the given symbol, resolution, and time range.
    ///
    /// Implementations are expected to write fetched data to the local
    /// data directory so subsequent requests hit disk rather than the
    /// network.
    fn get_trade_bars(
        &self,
        symbol: Symbol,
        resolution: Resolution,
        start: DateTime,
        end: DateTime,
    ) -> Pin<Box<dyn Future<Output = LeanResult<Vec<TradeBar>>> + Send + '_>>;
}
