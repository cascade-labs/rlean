use async_trait::async_trait;
use rlean_core::{Market, OptionRight, OptionStyle, Symbol, SymbolOptionsExt};
use rlean_data::{MarginInterestRate, PerpetualContext, QuoteBar, Tick, TradeBar};
use rlean_storage::{FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow};
use std::collections::{HashMap, HashSet};
use tracing::debug;

use crate::request::{
    DataType, DownloadRequest, HistoryBatchRequest, HistoryRequest, MarketDataBatch,
    OptionDataType, OptionHistoryBatchRequest, OptionMarketDataBatch,
};

pub type TickStream = Box<dyn Iterator<Item = anyhow::Result<Tick>> + Send>;

fn option_right_from_row(row: &OptionUniverseRow) -> Option<OptionRight> {
    match row.right.to_ascii_uppercase().as_str() {
        "C" | "CALL" => Some(OptionRight::Call),
        "P" | "PUT" => Some(OptionRight::Put),
        _ => None,
    }
}

fn option_symbol_from_universe_row(ticker: &str, row: &OptionUniverseRow) -> Option<Symbol> {
    let right = option_right_from_row(row)?;
    let underlying = Symbol::create_equity(ticker, &Market::usa());
    Some(Symbol::create_option_osi(
        underlying,
        row.strike,
        row.expiration,
        right,
        OptionStyle::American,
        &Market::usa(),
    ))
}

fn option_history_request(
    ticker: &str,
    row: &OptionUniverseRow,
    resolution: rlean_core::Resolution,
    date: chrono::NaiveDate,
    data_type: DataType,
) -> Option<HistoryRequest> {
    let start = rlean_core::DateTime::from(date.and_hms_opt(0, 0, 0)?);
    let end = rlean_core::DateTime::from(date.succ_opt()?.and_hms_opt(0, 0, 0)?);
    Some(HistoryRequest {
        symbol: option_symbol_from_universe_row(ticker, row)?,
        resolution,
        start,
        end,
        data_type,
    })
}

/// Provides historical market data — Rust equivalent of C# `IHistoryProvider`.
///
/// Implementors are expected to fetch data from a remote source (or local
/// disk), write it to the Parquet store, and return the raw bars.
#[async_trait]
pub trait IHistoryProvider: Send + Sync {
    /// Human-readable provider name used in verbose diagnostics.
    fn name(&self) -> &str {
        std::any::type_name::<Self>()
    }

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

    /// Fetch the corporate-action factor file (split/dividend adjustments) for
    /// `symbol`, returning the rows for the framework to persist.
    ///
    /// Providers are pure data sources: they fetch/compute the rows and return
    /// them. The framework owns all persistence — it writes the returned rows
    /// into the Iceberg `factor_files` table exactly as it does trade/quote
    /// bars. Providers MUST NOT write files themselves.
    ///
    /// Returns `Ok(vec![])` (default) when the provider has no corporate-action
    /// source for the symbol; the framework treats that as "nothing to persist".
    ///
    /// CONTRACT — partial failure MUST return `Err`. If any upstream call needed
    /// to build the factor file fails (a splits/dividends fetch errors or rate
    /// limits), the provider MUST return `Err`, not a synthesized or default
    /// factor file built from the data it did manage to get. The framework
    /// persists any non-empty `Ok` result and never refetches once rows exist,
    /// so returning fabricated rows after an upstream error poisons the cache
    /// permanently — every later run reads the wrong factors and skips real
    /// split/dividend adjustment. On `Err` the framework persists nothing and
    /// retries on the next run. Only synthesize a default/identity file when
    /// every fetch succeeded and the symbol genuinely has no corporate actions.
    async fn get_factor_file(&self, _symbol: &Symbol) -> anyhow::Result<Vec<FactorFileEntry>> {
        Ok(vec![])
    }

    /// Fetch the map file (ticker rename / listing history) for `symbol`,
    /// returning the rows for the framework to persist into the Iceberg
    /// `map_files` table. Providers MUST NOT write files themselves.
    ///
    /// Returns `Ok(vec![])` (default) when the provider has no ticker-detail
    /// source for the symbol.
    ///
    /// CONTRACT — partial failure MUST return `Err`. Same rule as
    /// `get_factor_file`: if any upstream call needed to build the map file
    /// fails (e.g. the ticker-events fetch errors or rate limits), the provider
    /// MUST return `Err`, not a default map built from partial data. The
    /// framework persists any non-empty `Ok` result and never refetches once
    /// rows exist, so a default map returned after a failed events fetch
    /// permanently erases a ticker's rename history (e.g. FB -> META) from the
    /// cache. On `Err` the framework persists nothing and retries next run.
    /// Only synthesize a default map when every fetch succeeded and the symbol
    /// genuinely has no rename events.
    async fn get_map_file(&self, _symbol: &Symbol) -> anyhow::Result<Vec<MapFileEntry>> {
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
            let result: anyhow::Result<()> = match request.data_type {
                DataType::TradeBar | DataType::FactorFile | DataType::MapFile => {
                    match self.get_history(&single).await {
                        Ok(rows) => {
                            batch.trade_bars.extend(rows);
                            Ok(())
                        }
                        Err(err) => Err(err),
                    }
                }
                DataType::QuoteBar => match self.get_quote_bars(&single).await {
                    Ok(rows) => {
                        batch.quote_bars.extend(rows);
                        Ok(())
                    }
                    Err(err) => Err(err),
                },
                DataType::Tick | DataType::OpenInterest => match self.get_ticks(&single).await {
                    Ok(rows) => {
                        batch.ticks.extend(rows);
                        Ok(())
                    }
                    Err(err) => Err(err),
                },
                DataType::MarginInterestRate => match self.get_margin_interest_rates(&single).await
                {
                    Ok(rows) => {
                        batch.margin_interest_rates.extend(rows);
                        Ok(())
                    }
                    Err(err) => Err(err),
                },
                DataType::PerpetualContext => match self.get_perpetual_contexts(&single).await {
                    Ok(rows) => {
                        batch.perpetual_contexts.extend(rows);
                        Ok(())
                    }
                    Err(err) => Err(err),
                },
            };
            if let Err(err) = result {
                debug!(
                    "Skipping failed {:?} batch symbol {} ({} → {}): {}",
                    request.data_type,
                    symbol.value,
                    request.start.date_utc(),
                    request.end.date_utc(),
                    err
                );
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

    /// Fetch option universes for a set of underlyings on `date`.
    ///
    /// Providers that can batch chain lookups should override this. The default
    /// preserves existing provider behavior by fanning out to the one-underlying
    /// method.
    async fn get_option_universes(
        &self,
        tickers: &[String],
        date: chrono::NaiveDate,
    ) -> anyhow::Result<HashMap<String, Vec<OptionUniverseRow>>> {
        let mut out = HashMap::new();
        let mut seen = HashSet::new();
        for ticker in tickers {
            let key = ticker.to_ascii_uppercase();
            if !seen.insert(key.clone()) {
                continue;
            }
            out.insert(key, self.get_option_universe(ticker, date).await?);
        }
        Ok(out)
    }

    /// Fetch intraday option trade bars for all contracts of `ticker` on `date`.
    async fn get_option_trade_bars(
        &self,
        _ticker: &str,
        _resolution: rlean_core::Resolution,
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
        resolution: rlean_core::Resolution,
        date: chrono::NaiveDate,
        contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<Vec<TradeBar>> {
        let mut bars = Vec::new();
        for contract in contracts {
            let Some(request) =
                option_history_request(ticker, contract, resolution, date, DataType::TradeBar)
            else {
                continue;
            };
            bars.extend(self.get_history(&request).await?);
        }
        Ok(bars)
    }

    /// Fetch intraday option quote bars for all contracts of `ticker` on `date`.
    async fn get_option_quote_bars(
        &self,
        _ticker: &str,
        _resolution: rlean_core::Resolution,
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
        resolution: rlean_core::Resolution,
        date: chrono::NaiveDate,
        contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<Vec<QuoteBar>> {
        let mut bars = Vec::new();
        for contract in contracts {
            let Some(request) =
                option_history_request(ticker, contract, resolution, date, DataType::QuoteBar)
            else {
                continue;
            };
            bars.extend(self.get_quote_bars(&request).await?);
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
        contracts: &[OptionUniverseRow],
    ) -> anyhow::Result<Vec<Tick>> {
        let mut ticks = Vec::new();
        for contract in contracts {
            let Some(request) = option_history_request(
                ticker,
                contract,
                rlean_core::Resolution::Tick,
                date,
                DataType::Tick,
            ) else {
                continue;
            };
            ticks.extend(self.get_ticks(&request).await?);
        }
        Ok(ticks)
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
            let result: anyhow::Result<()> = match request.data_type {
                OptionDataType::EodBar => {
                    match self.get_option_eod_bars(ticker, request.date).await {
                        Ok(rows) => {
                            batch.eod_bars.extend(rows);
                            Ok(())
                        }
                        Err(err) => Err(err),
                    }
                }
                OptionDataType::Universe => {
                    match self.get_option_universe(ticker, request.date).await {
                        Ok(rows) => {
                            batch.universe.extend(rows);
                            Ok(())
                        }
                        Err(err) => Err(err),
                    }
                }
                OptionDataType::TradeBar => {
                    match self.get_option_universe(ticker, request.date).await {
                        Ok(contracts) => match self
                            .get_option_trade_bars_filtered(
                                ticker,
                                request.resolution,
                                request.date,
                                &contracts,
                            )
                            .await
                        {
                            Ok(rows) => {
                                batch.trade_bars.extend(rows);
                                Ok(())
                            }
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                }
                OptionDataType::QuoteBar => {
                    match self.get_option_universe(ticker, request.date).await {
                        Ok(contracts) => match self
                            .get_option_quote_bars_filtered(
                                ticker,
                                request.resolution,
                                request.date,
                                &contracts,
                            )
                            .await
                        {
                            Ok(rows) => {
                                batch.quote_bars.extend(rows);
                                Ok(())
                            }
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                }
                OptionDataType::Tick => {
                    match self.get_option_universe(ticker, request.date).await {
                        Ok(contracts) => match self
                            .get_option_ticks_filtered(ticker, request.date, &contracts)
                            .await
                        {
                            Ok(rows) => {
                                batch.ticks.extend(rows);
                                Ok(())
                            }
                            Err(err) => Err(err),
                        },
                        Err(err) => Err(err),
                    }
                }
            };
            if let Err(err) = result {
                debug!(
                    "Skipping failed option {:?} batch ticker {} ({}): {}",
                    request.data_type, ticker, request.date, err
                );
            }
        }
        Ok(batch)
    }

    /// The earliest date this provider can supply data for, if limited.
    ///
    /// The data feed uses this to clip requested date ranges before making
    /// network calls.
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

/// Live data providers implement LEAN's `IDataQueueHandler` contract.
pub trait ILiveDataProvider: rlean_data::DataQueueHandler {}

impl<T> ILiveDataProvider for T where T: rlean_data::DataQueueHandler {}
