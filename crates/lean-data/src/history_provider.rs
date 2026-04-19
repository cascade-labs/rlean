use crate::TradeBar;
use lean_core::{DateTime, Resolution, Result as LeanResult, Symbol};
use std::future::Future;
use std::pin::Pin;

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

    /// The earliest date this provider can supply data for, if limited.
    ///
    /// When `Some(date)` is returned the framework clips the requested start
    /// to this date before calling `get_trade_bars`, preventing subscription-
    /// tier errors (e.g. ThetaData STANDARD only covers data from 2018-01-01).
    /// Returns `None` (default) when the provider has no known lower bound.
    fn earliest_date(&self) -> Option<chrono::NaiveDate> {
        None
    }
}
