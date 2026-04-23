use async_trait::async_trait;
use lean_core::Symbol;
use lean_data::{QuoteBar, Tick, TradeBar};
use lean_storage::{FactorFileEntry, OptionEodBar, OptionUniverseRow};

use crate::request::{DownloadRequest, HistoryRequest};

/// Provides historical market data — Rust equivalent of C# `IHistoryProvider`.
///
/// Implementors are expected to fetch data from a remote source (or local
/// disk), write it to the Parquet store, and return the raw bars.
///
/// This trait is **synchronous** by design.  Plugins are loaded as cdylib
/// dynamic libraries; each plugin links its own copy of tokio and cannot share
/// runtime state (thread-locals) with the host binary.  Making the trait sync
/// lets plugins block internally (e.g. via a `current_thread` runtime or
/// `reqwest::blocking`) while the host adapts the call to async via
/// `tokio::task::spawn_blocking`.
pub trait IHistoryProvider: Send + Sync {
    /// Fetch historical trade bars for the symbol described in `request`.
    fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>>;

    /// Fetch historical quote bars for the symbol described in `request`.
    fn get_quote_bars(&self, _request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        Ok(vec![])
    }

    /// Fetch historical ticks for the symbol described in `request`.
    fn get_ticks(&self, _request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        Ok(vec![])
    }

    /// Fetch all option EOD bars for `ticker` on `date`.
    ///
    /// Returns an empty vec if this provider does not support option data.
    /// Providers that do (e.g. ThetaData) override this to fetch from their
    /// source and cache locally.  The host runner calls this through
    /// `tokio::task::spawn_blocking` since the trait is sync.
    fn get_option_eod_bars(
        &self,
        _ticker: &str,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<OptionEodBar>> {
        Ok(vec![])
    }

    /// Fetch the option universe for `ticker` on `date`.
    ///
    /// Returned rows identify which contracts existed for the underlying on the
    /// requested date. Intraday option minute/tick paths use this to reconstruct
    /// symbols and build chains without falling back to daily EOD snapshots.
    fn get_option_universe(
        &self,
        _ticker: &str,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<OptionUniverseRow>> {
        Ok(vec![])
    }

    /// Fetch intraday option trade bars for all contracts of `ticker` on `date`.
    fn get_option_trade_bars(
        &self,
        _ticker: &str,
        _resolution: lean_core::Resolution,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        Ok(vec![])
    }

    /// Fetch intraday option quote bars for all contracts of `ticker` on `date`.
    fn get_option_quote_bars(
        &self,
        _ticker: &str,
        _resolution: lean_core::Resolution,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<QuoteBar>> {
        Ok(vec![])
    }

    /// Fetch option ticks for all contracts of `ticker` on `date`.
    fn get_option_ticks(
        &self,
        _ticker: &str,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<Tick>> {
        Ok(vec![])
    }

    /// The earliest date this provider can supply data for, if limited.
    ///
    /// The async adapter (`HistoryProviderAdapter`) forwards this to
    /// `IHistoricalDataProvider::earliest_date` so the runner can clip
    /// requested date ranges before making network calls.
    /// Returns `None` (default) when the provider has no known lower bound.
    fn earliest_date(&self) -> Option<chrono::NaiveDate> {
        None
    }
}

/// Downloads and persists data to the local Parquet store.
/// Rust equivalent of C# `IDataDownloader`.
#[async_trait]
pub trait IDataDownloader: Send + Sync {
    /// Download data for the given request and write it to the local store.
    /// Returns the number of bars written.
    async fn download(&self, request: &DownloadRequest) -> anyhow::Result<usize>;
}

/// Provides the full option contract list for an underlying on a given date.
/// Rust equivalent of C# `IOptionChainProvider`.
pub trait IOptionChainProvider: Send + Sync {
    /// Return all option contract symbols for `underlying` on `date`.
    fn get_option_contract_list(
        &self,
        underlying: &Symbol,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<Symbol>>;
}

/// Provides split/dividend adjustment factor files.
/// Rust equivalent of C# `IFactorFileProvider`.
pub trait IFactorFileProvider: Send + Sync {
    /// Return the factor-file rows for `symbol`, or `None` if not available.
    fn get(&self, symbol: &Symbol) -> Option<Vec<FactorFileEntry>>;
}

/// Provides ticker-to-SID mapping files (handles renames/delistings).
/// Rust equivalent of C# `IMapFileProvider`.
pub trait IMapFileProvider: Send + Sync {
    /// Return the current ticker for `symbol` on `date`, or `None` if unmapped.
    fn get(&self, symbol: &Symbol, date: chrono::NaiveDate) -> Option<String>;
}

/// Subscribes to a live data stream — Rust equivalent of C# `IDataQueueHandler`.
#[async_trait]
pub trait ILiveDataProvider: Send + Sync {
    /// Subscribe to live data for `symbol`.
    async fn subscribe(&self, symbol: &Symbol) -> anyhow::Result<()>;

    /// Unsubscribe from live data for `symbol`.
    async fn unsubscribe(&self, symbol: &Symbol) -> anyhow::Result<()>;

    /// Whether the provider is currently connected to the live feed.
    fn is_connected(&self) -> bool;
}
