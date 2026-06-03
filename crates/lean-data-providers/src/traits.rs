use async_trait::async_trait;
use lean_core::Symbol;
use lean_data::{MarginInterestRate, PerpetualContext, QuoteBar, Tick, TradeBar};
use lean_storage::{FactorFileEntry, OptionEodBar, OptionUniverseRow};

use crate::request::{
    DataType, DownloadRequest, HistoryBatchRequest, HistoryRequest, MarketDataBatch,
    OptionDataType, OptionHistoryBatchRequest, OptionMarketDataBatch,
};

pub type TickStream = Box<dyn Iterator<Item = anyhow::Result<Tick>> + Send>;

/// Provides historical market data — Rust equivalent of C# `IHistoryProvider`.
///
/// Implementors are expected to fetch data from a remote source (or local
/// disk), write it to the Parquet store, and return the raw bars.
#[async_trait]
pub trait IHistoryProvider: Send + Sync {
    /// Fetch historical trade bars for the symbol described in `request`.
    async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>>;

    /// Fetch historical quote bars for the symbol described in `request`.
    async fn get_quote_bars(&self, _request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        Ok(vec![])
    }

    /// Fetch historical ticks for the symbol described in `request`.
    async fn get_ticks(&self, _request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        Ok(vec![])
    }

    /// Fetch historical margin-interest/funding-rate data for the symbol.
    async fn get_margin_interest_rates(
        &self,
        _request: &HistoryRequest,
    ) -> anyhow::Result<Vec<MarginInterestRate>> {
        Ok(vec![])
    }

    /// Fetch historical perpetual context data for the symbol.
    async fn get_perpetual_contexts(
        &self,
        _request: &HistoryRequest,
    ) -> anyhow::Result<Vec<PerpetualContext>> {
        Ok(vec![])
    }

    /// Fetch/cache a multi-symbol batch. Providers with true batch APIs should
    /// override this; the default keeps existing providers correct by fanning
    /// out over the single-symbol async methods.
    async fn get_history_batch(
        &self,
        request: &HistoryBatchRequest,
    ) -> anyhow::Result<MarketDataBatch> {
        let mut batch = MarketDataBatch::default();
        for symbol in &request.symbols {
            let single = HistoryRequest {
                symbol: symbol.clone(),
                resolution: request.resolution,
                start: request.start,
                end: request.end,
                data_type: request.data_type,
            };
            match request.data_type {
                DataType::TradeBar | DataType::FactorFile | DataType::MapFile => {
                    batch.trade_bars.extend(self.get_history(&single).await?);
                }
                DataType::QuoteBar => {
                    batch.quote_bars.extend(self.get_quote_bars(&single).await?);
                }
                DataType::Tick | DataType::OpenInterest => {
                    batch.ticks.extend(self.get_ticks(&single).await?);
                }
                DataType::MarginInterestRate => {
                    batch
                        .margin_interest_rates
                        .extend(self.get_margin_interest_rates(&single).await?);
                }
                DataType::PerpetualContext => {
                    batch
                        .perpetual_contexts
                        .extend(self.get_perpetual_contexts(&single).await?);
                }
            }
        }
        Ok(batch)
    }

    /// Fetch all option EOD bars for `ticker` on `date`.
    ///
    /// Returns an empty vec if this provider does not support option data.
    /// Providers that do (e.g. ThetaData) override this to fetch from their
    /// source and cache locally.
    async fn get_option_eod_bars(
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
    async fn get_option_universe(
        &self,
        _ticker: &str,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<OptionUniverseRow>> {
        Ok(vec![])
    }

    /// Fetch intraday option trade bars for all contracts of `ticker` on `date`.
    async fn get_option_trade_bars(
        &self,
        _ticker: &str,
        _resolution: lean_core::Resolution,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        Ok(vec![])
    }

    /// Fetch intraday option trade bars constrained to an already-selected option universe.
    /// Providers with contract-specific chain endpoints should override this so option
    /// filters reduce the remote request surface and include held contracts explicitly.
    async fn get_option_trade_bars_filtered(
        &self,
        ticker: &str,
        resolution: lean_core::Resolution,
        date: chrono::NaiveDate,
        contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<Vec<TradeBar>> {
        let allowed = contracts
            .iter()
            .map(|row| row.symbol_value.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut bars = self.get_option_trade_bars(ticker, resolution, date).await?;
        if !allowed.is_empty() {
            bars.retain(|bar| allowed.contains(bar.symbol.value.as_str()));
        }
        Ok(bars)
    }

    /// Fetch intraday option quote bars for all contracts of `ticker` on `date`.
    async fn get_option_quote_bars(
        &self,
        _ticker: &str,
        _resolution: lean_core::Resolution,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<QuoteBar>> {
        Ok(vec![])
    }

    /// Fetch intraday option quote bars constrained to an already-selected option universe.
    /// Providers with contract-specific chain endpoints should override this so option
    /// filters reduce the remote request surface and include held contracts explicitly.
    async fn get_option_quote_bars_filtered(
        &self,
        ticker: &str,
        resolution: lean_core::Resolution,
        date: chrono::NaiveDate,
        contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<Vec<QuoteBar>> {
        let allowed = contracts
            .iter()
            .map(|row| row.symbol_value.as_str())
            .collect::<std::collections::HashSet<_>>();
        let mut bars = self.get_option_quote_bars(ticker, resolution, date).await?;
        if !allowed.is_empty() {
            bars.retain(|bar| allowed.contains(bar.symbol.value.as_str()));
        }
        Ok(bars)
    }

    /// Fetch option ticks for all contracts of `ticker` on `date`.
    async fn get_option_ticks(
        &self,
        _ticker: &str,
        _date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<Tick>> {
        Ok(vec![])
    }

    /// Fetch option ticks constrained to an already-selected option universe.
    /// Providers with chain endpoints should override this so LEAN-style option
    /// filters reduce the remote request surface, not only the delivered data.
    async fn get_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        _contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<Vec<Tick>> {
        self.get_option_ticks(ticker, date).await
    }

    /// Open a memory-bounded stream of option ticks constrained to an
    /// already-selected option universe. Implementors should yield ticks in
    /// timestamp order. The default preserves provider compatibility by
    /// delegating to the batch method.
    async fn stream_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<TickStream> {
        let ticks = self
            .get_option_ticks_filtered(ticker, date, contracts)
            .await?;
        Ok(Box::new(ticks.into_iter().map(Ok)))
    }

    /// Fetch/cache option data for several underlyings on one trading day.
    /// Providers with true bulk APIs should override this; the default keeps
    /// existing providers correct by fanning out over the single-underlying
    /// methods sequentially.
    async fn get_option_history_batch(
        &self,
        request: &OptionHistoryBatchRequest,
    ) -> anyhow::Result<OptionMarketDataBatch> {
        let mut batch = OptionMarketDataBatch::default();
        for ticker in &request.tickers {
            match request.data_type {
                OptionDataType::EodBar => {
                    batch
                        .eod_bars
                        .extend(self.get_option_eod_bars(ticker, request.date).await?);
                }
                OptionDataType::Universe => {
                    batch
                        .universe
                        .extend(self.get_option_universe(ticker, request.date).await?);
                }
                OptionDataType::TradeBar => {
                    batch.trade_bars.extend(
                        self.get_option_trade_bars(ticker, request.resolution, request.date)
                            .await?,
                    );
                }
                OptionDataType::QuoteBar => {
                    batch.quote_bars.extend(
                        self.get_option_quote_bars(ticker, request.resolution, request.date)
                            .await?,
                    );
                }
                OptionDataType::Tick => {
                    batch
                        .ticks
                        .extend(self.get_option_ticks(ticker, request.date).await?);
                }
            }
        }
        Ok(batch)
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
