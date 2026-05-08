/// Stacked (priority-ordered) history provider.
///
/// Tries each provider in order.  The first provider that returns a non-empty
/// `Ok` result wins.  For side-effect requests (factor/map files), every
/// provider that does not return `NotImplemented:` is given a chance to write
/// its file because success is represented by a filesystem side effect, not
/// returned rows.  A provider that returns `Ok(vec![])` for market data or an
/// `anyhow::Error` whose message starts with "NotImplemented:" is treated as
/// "I don't have this data — try the next one".  Any other error short-circuits
/// and is returned immediately.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use lean_core::Resolution;
use lean_data::{QuoteBar, Tick, TradeBar};
use lean_storage::{OptionEodBar, OptionUniverseRow};
use tracing::debug;

use crate::{
    DataType, HistoryBatchRequest, HistoryRequest, IHistoryProvider, MarketDataBatch,
    OptionDataType, OptionHistoryBatchRequest, OptionMarketDataBatch, TickStream,
};

/// Returns `true` when `err` indicates that the provider does not implement
/// the requested data type (as opposed to a transient network or parse error).
pub fn is_not_implemented(err: &anyhow::Error) -> bool {
    err.to_string().starts_with("NotImplemented:")
}

fn market_data_batch_is_empty(batch: &MarketDataBatch, data_type: DataType) -> bool {
    match data_type {
        DataType::TradeBar | DataType::FactorFile | DataType::MapFile => {
            batch.trade_bars.is_empty()
        }
        DataType::QuoteBar => batch.quote_bars.is_empty(),
        DataType::Tick | DataType::OpenInterest => batch.ticks.is_empty(),
    }
}

fn is_side_effect_data_type(data_type: DataType) -> bool {
    matches!(data_type, DataType::FactorFile | DataType::MapFile)
}

fn option_market_data_batch_is_empty(
    batch: &OptionMarketDataBatch,
    data_type: OptionDataType,
) -> bool {
    match data_type {
        OptionDataType::EodBar => batch.eod_bars.is_empty(),
        OptionDataType::Universe => batch.universe.is_empty(),
        OptionDataType::TradeBar => batch.trade_bars.is_empty(),
        OptionDataType::QuoteBar => batch.quote_bars.is_empty(),
        OptionDataType::Tick => batch.ticks.is_empty(),
    }
}

/// Wraps multiple `IHistoryProvider` implementations and tries them in
/// priority order.
pub struct StackedHistoryProvider {
    providers: Vec<Arc<dyn IHistoryProvider>>,
    option_universe_cache: Mutex<HashMap<String, OptionUniverseCacheEntry>>,
}

struct OptionUniverseCacheEntry {
    date: chrono::NaiveDate,
    rows: Arc<Vec<OptionUniverseRow>>,
}

impl StackedHistoryProvider {
    /// Create a new stacked provider.  `providers` must be non-empty and are
    /// tried left-to-right (index 0 = highest priority).
    pub fn new(providers: Vec<Arc<dyn IHistoryProvider>>) -> Self {
        assert!(
            !providers.is_empty(),
            "StackedHistoryProvider requires at least one provider"
        );
        StackedHistoryProvider {
            providers,
            option_universe_cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl IHistoryProvider for StackedHistoryProvider {
    async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
        for (idx, provider) in self.providers.iter().enumerate() {
            match provider.get_history(request).await {
                Ok(data) if !data.is_empty() => {
                    debug!(
                        "History provider #{} returned {} {:?} rows for {} ({} → {})",
                        idx,
                        data.len(),
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    return Ok(data);
                }
                Ok(_data) if is_side_effect_data_type(request.data_type) => {
                    debug!(
                        "History provider #{} accepted {:?} for {} as a side-effect ({} → {}); trying remaining providers too",
                        idx,
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Ok(_) => {
                    debug!(
                        "History provider #{} returned 0 {:?} rows for {} ({} → {})",
                        idx,
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Err(ref e) if is_not_implemented(e) => {
                    debug!(
                        "History provider #{} does not implement {:?} for {}",
                        idx, request.data_type, request.symbol.value
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_quote_bars(&self, request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        for provider in &self.providers {
            match provider.get_quote_bars(request).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_ticks(&self, request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        for provider in &self.providers {
            match provider.get_ticks(request).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_history_batch(
        &self,
        request: &HistoryBatchRequest,
    ) -> anyhow::Result<MarketDataBatch> {
        for (idx, provider) in self.providers.iter().enumerate() {
            match provider.get_history_batch(request).await {
                Ok(data) if !market_data_batch_is_empty(&data, request.data_type) => {
                    debug!(
                        "History provider #{} returned batched {:?} rows for {} symbols ({} → {})",
                        idx,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    return Ok(data);
                }
                Ok(_data) if is_side_effect_data_type(request.data_type) => {
                    debug!(
                        "History provider #{} accepted batched {:?} for {} symbols as a side-effect ({} → {}); trying remaining providers too",
                        idx,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Ok(_) => {
                    debug!(
                        "History provider #{} returned 0 batched {:?} rows for {} symbols ({} → {})",
                        idx,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Err(ref e) if is_not_implemented(e) => {
                    debug!(
                        "History provider #{} does not implement batched {:?}",
                        idx, request.data_type
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(MarketDataBatch::default())
    }

    async fn get_option_eod_bars(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<OptionEodBar>> {
        for provider in &self.providers {
            match provider.get_option_eod_bars(ticker, date).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_option_universe(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<OptionUniverseRow>> {
        let key = ticker.to_ascii_uppercase();
        if let Some(rows) = self
            .option_universe_cache
            .lock()
            .expect("option universe cache poisoned")
            .get(&key)
            .filter(|entry| entry.date == date)
            .map(|entry| Arc::clone(&entry.rows))
        {
            return Ok(rows.as_ref().clone());
        }

        let mut rows = Vec::new();
        for provider in &self.providers {
            match provider.get_option_universe(ticker, date).await {
                Ok(data) if !data.is_empty() => {
                    rows = data;
                    break;
                }
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }

        self.option_universe_cache
            .lock()
            .expect("option universe cache poisoned")
            .insert(
                key,
                OptionUniverseCacheEntry {
                    date,
                    rows: Arc::new(rows.clone()),
                },
            );

        Ok(rows)
    }

    async fn get_option_trade_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        for provider in &self.providers {
            match provider
                .get_option_trade_bars(ticker, resolution, date)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_option_quote_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<QuoteBar>> {
        for provider in &self.providers {
            match provider
                .get_option_quote_bars(ticker, resolution, date)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_option_ticks(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<Tick>> {
        for provider in &self.providers {
            match provider.get_option_ticks(ticker, date).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<Tick>> {
        for provider in &self.providers {
            match provider
                .get_option_ticks_filtered(ticker, date, contracts)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn stream_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<TickStream> {
        for provider in &self.providers {
            match provider
                .stream_option_ticks_filtered(ticker, date, contracts)
                .await
            {
                Ok(mut stream) => match stream.next() {
                    Some(Ok(first_tick)) => {
                        return Ok(Box::new(std::iter::once(Ok(first_tick)).chain(stream)));
                    }
                    Some(Err(e)) if is_not_implemented(&e) => continue,
                    Some(Err(e)) => return Err(e),
                    None => continue,
                },
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(Box::new(std::iter::empty()))
    }

    async fn get_option_history_batch(
        &self,
        request: &OptionHistoryBatchRequest,
    ) -> anyhow::Result<OptionMarketDataBatch> {
        for (idx, provider) in self.providers.iter().enumerate() {
            match provider.get_option_history_batch(request).await {
                Ok(data) if !option_market_data_batch_is_empty(&data, request.data_type) => {
                    debug!(
                        "History provider #{} returned batched option {:?} rows for {} tickers ({})",
                        idx,
                        request.data_type,
                        request.tickers.len(),
                        request.date
                    );
                    return Ok(data);
                }
                Ok(_) => {
                    debug!(
                        "History provider #{} returned 0 batched option {:?} rows for {} tickers ({})",
                        idx,
                        request.data_type,
                        request.tickers.len(),
                        request.date
                    );
                    continue;
                }
                Err(ref e) if is_not_implemented(e) => {
                    debug!(
                        "History provider #{} does not implement batched option {:?}",
                        idx, request.data_type
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(OptionMarketDataBatch::default())
    }

    fn earliest_date(&self) -> Option<chrono::NaiveDate> {
        self.providers
            .iter()
            .filter_map(|p| p.earliest_date())
            .min()
    }
}
