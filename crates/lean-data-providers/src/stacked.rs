/// Stacked (priority-ordered) history provider.
///
/// Tries each provider in order.  The first provider that returns a non-empty
/// `Ok` result wins.  For side-effect requests (factor/map files), every
/// provider that does not return `NotImplemented:` is given a chance to write
/// its file because success is represented by a filesystem side effect, not
/// returned rows.  A provider that returns `Ok(vec![])` for market data or an
/// `anyhow::Error` whose message starts with "NotImplemented:" is treated as
/// "I don't have this data — try the next one".  With a single provider,
/// unexpected provider errors are returned. With multiple providers, a provider
/// error is treated as a miss so fallback providers can satisfy the request.
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use lean_core::{Market, OptionRight, OptionStyle, Resolution, Symbol, SymbolOptionsExt};
use lean_data::{MarginInterestRate, PerpetualContext, QuoteBar, Tick, TradeBar};
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

fn is_recoverable_cache_error(err: &anyhow::Error) -> bool {
    let message = err.to_string();
    message.contains("Parquet error") || message.contains("Invalid Parquet file")
}

fn should_fall_through_provider_error(provider_count: usize) -> bool {
    provider_count > 1
}

fn provider_label(idx: usize, provider: &Arc<dyn IHistoryProvider>) -> String {
    format!("{} #{}", provider.name(), idx)
}

fn market_data_batch_is_empty(batch: &MarketDataBatch, data_type: DataType) -> bool {
    match data_type {
        DataType::TradeBar | DataType::FactorFile | DataType::MapFile => {
            batch.trade_bars.is_empty()
        }
        DataType::QuoteBar => batch.quote_bars.is_empty(),
        DataType::Tick | DataType::OpenInterest => batch.ticks.is_empty(),
        DataType::MarginInterestRate => batch.margin_interest_rates.is_empty(),
        DataType::PerpetualContext => batch.perpetual_contexts.is_empty(),
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

fn option_quote_bars_cover_contracts(
    ticker: &str,
    bars: &[QuoteBar],
    contracts: &[OptionUniverseRow],
) -> bool {
    if contracts.is_empty() {
        return true;
    }
    let required = option_contract_symbol_values(ticker, contracts);
    if required.is_empty() {
        return true;
    }
    let available = bars
        .iter()
        .map(|bar| bar.symbol.value.as_ref())
        .collect::<std::collections::HashSet<_>>();
    required
        .iter()
        .all(|symbol_value| available.contains(symbol_value.as_str()))
}

fn option_contract_symbol_values(ticker: &str, contracts: &[OptionUniverseRow]) -> Vec<String> {
    let underlying = Symbol::create_equity(ticker, &Market::usa());
    contracts
        .iter()
        .filter_map(|row| {
            let right = match row.right.to_ascii_uppercase().as_str() {
                "C" | "CALL" => OptionRight::Call,
                "P" | "PUT" => OptionRight::Put,
                _ => return None,
            };
            Some(
                Symbol::create_option_osi(
                    underlying.clone(),
                    row.strike,
                    row.expiration,
                    right,
                    OptionStyle::American,
                    &Market::usa(),
                )
                .value
                .to_string(),
            )
        })
        .collect()
}

fn normalize_underlying(ticker: &str) -> String {
    ticker.trim().trim_start_matches('?').to_ascii_uppercase()
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
            let provider_label = provider_label(idx, provider);
            match provider.get_history(request).await {
                Ok(data) if !data.is_empty() => {
                    debug!(
                        "History provider {} returned {} {:?} rows for {} ({} → {})",
                        provider_label,
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
                        "History provider {} accepted {:?} for {} as a side-effect ({} → {}); trying remaining providers too",
                        provider_label,
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Ok(_) => {
                    debug!(
                        "History provider {} returned 0 {:?} rows for {} ({} → {})",
                        provider_label,
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Err(ref e) if is_not_implemented(e) => {
                    debug!(
                        "History provider {} does not implement {:?} for {}",
                        provider_label, request.data_type, request.symbol.value
                    );
                    continue;
                }
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable cache error for {:?} {} ({} → {}): {}",
                        provider_label,
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc(),
                        e
                    );
                    continue;
                }
                Err(e) if should_fall_through_provider_error(self.providers.len()) => {
                    debug!(
                        "History provider {} failed for {:?} {} ({} → {}); trying next provider: {}",
                        provider_label,
                        request.data_type,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc(),
                        e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_quote_bars(&self, request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider.get_quote_bars(request).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable quote cache error for {}: {}",
                        provider_label, request.symbol.value, e
                    );
                    continue;
                }
                Err(e) if should_fall_through_provider_error(self.providers.len()) => {
                    debug!(
                        "History provider {} failed for quote bars {} ({} → {}); trying next provider: {}",
                        provider_label,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc(),
                        e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_ticks(&self, request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider.get_ticks(request).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable tick cache error for {}: {}",
                        provider_label, request.symbol.value, e
                    );
                    continue;
                }
                Err(e) if should_fall_through_provider_error(self.providers.len()) => {
                    debug!(
                        "History provider {} failed for ticks {} ({} → {}); trying next provider: {}",
                        provider_label,
                        request.symbol.value,
                        request.start.date_utc(),
                        request.end.date_utc(),
                        e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_margin_interest_rates(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<MarginInterestRate>> {
        for provider in &self.providers {
            match provider.get_margin_interest_rates(request).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_perpetual_contexts(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<PerpetualContext>> {
        for provider in &self.providers {
            match provider.get_perpetual_contexts(request).await {
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
            let provider_label = provider_label(idx, provider);
            match provider.get_history_batch(request).await {
                Ok(data) if !market_data_batch_is_empty(&data, request.data_type) => {
                    debug!(
                        "History provider {} returned batched {:?} rows for {} symbols ({} → {})",
                        provider_label,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    return Ok(data);
                }
                Ok(_data) if is_side_effect_data_type(request.data_type) => {
                    debug!(
                        "History provider {} accepted batched {:?} for {} symbols as a side-effect ({} → {}); trying remaining providers too",
                        provider_label,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Ok(_) => {
                    debug!(
                        "History provider {} returned 0 batched {:?} rows for {} symbols ({} → {})",
                        provider_label,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc()
                    );
                    continue;
                }
                Err(ref e) if is_not_implemented(e) => {
                    debug!(
                        "History provider {} does not implement batched {:?}",
                        provider_label, request.data_type
                    );
                    continue;
                }
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable batched cache error for {:?}: {}",
                        provider_label, request.data_type, e
                    );
                    continue;
                }
                Err(e) if should_fall_through_provider_error(self.providers.len()) => {
                    debug!(
                        "History provider {} failed for batched {:?} ({} symbols, {} → {}); trying next provider: {}",
                        provider_label,
                        request.data_type,
                        request.symbols.len(),
                        request.start.date_utc(),
                        request.end.date_utc(),
                        e
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
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider.get_option_eod_bars(ticker, date).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable option EOD cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
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
        let key = normalize_underlying(ticker);
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

    async fn get_option_universes(
        &self,
        tickers: &[String],
        date: chrono::NaiveDate,
    ) -> anyhow::Result<HashMap<String, Vec<OptionUniverseRow>>> {
        let mut requested = Vec::new();
        let mut seen = HashSet::new();
        for ticker in tickers {
            let key = normalize_underlying(ticker);
            if key.is_empty() || !seen.insert(key.clone()) {
                continue;
            }
            requested.push(key);
        }

        let mut out = HashMap::new();
        let mut remaining = Vec::new();
        {
            let cache = self
                .option_universe_cache
                .lock()
                .expect("option universe cache poisoned");
            for ticker in requested {
                if let Some(rows) = cache
                    .get(&ticker)
                    .filter(|entry| entry.date == date)
                    .map(|entry| entry.rows.as_ref().clone())
                {
                    out.insert(ticker, rows);
                } else {
                    remaining.push(ticker);
                }
            }
        }

        for provider in &self.providers {
            if remaining.is_empty() {
                break;
            }

            match provider.get_option_universes(&remaining, date).await {
                Ok(batch) => {
                    let normalized_batch = batch
                        .into_iter()
                        .map(|(ticker, rows)| (normalize_underlying(&ticker), rows))
                        .collect::<HashMap<_, _>>();
                    let mut still_missing = Vec::new();
                    for ticker in remaining {
                        match normalized_batch.get(&ticker) {
                            Some(rows) if !rows.is_empty() => {
                                out.insert(ticker, rows.clone());
                            }
                            _ => still_missing.push(ticker),
                        }
                    }
                    remaining = still_missing;
                }
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }

        for ticker in remaining {
            out.entry(ticker).or_insert_with(Vec::new);
        }

        let mut cache = self
            .option_universe_cache
            .lock()
            .expect("option universe cache poisoned");
        for (ticker, rows) in &out {
            cache.insert(
                ticker.clone(),
                OptionUniverseCacheEntry {
                    date,
                    rows: Arc::new(rows.clone()),
                },
            );
        }

        Ok(out)
    }

    async fn get_option_trade_bars(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider
                .get_option_trade_bars(ticker, resolution, date)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable option trade cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_option_trade_bars_filtered(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<TradeBar>> {
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider
                .get_option_trade_bars_filtered(ticker, resolution, date, contracts)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable filtered option trade cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
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
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider
                .get_option_quote_bars(ticker, resolution, date)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable option quote cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }

    async fn get_option_quote_bars_filtered(
        &self,
        ticker: &str,
        resolution: Resolution,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<QuoteBar>> {
        let mut best_partial = Vec::new();
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider
                .get_option_quote_bars_filtered(ticker, resolution, date, contracts)
                .await
            {
                Ok(data)
                    if !data.is_empty()
                        && option_quote_bars_cover_contracts(ticker, &data, contracts) =>
                {
                    return Ok(data)
                }
                Ok(data) if !data.is_empty() => {
                    if data.len() > best_partial.len() {
                        best_partial = data;
                    }
                    continue;
                }
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable filtered option quote cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(best_partial)
    }

    async fn get_option_ticks(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<Tick>> {
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider.get_option_ticks(ticker, date).await {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable option tick cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
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
        for (idx, provider) in self.providers.iter().enumerate() {
            let provider_label = provider_label(idx, provider);
            match provider
                .get_option_ticks_filtered(ticker, date, contracts)
                .await
            {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable filtered option tick cache error for {} {}: {}",
                        provider_label, ticker, date, e
                    );
                    continue;
                }
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
            let provider_label = provider_label(idx, provider);
            match provider.get_option_history_batch(request).await {
                Ok(data) if !option_market_data_batch_is_empty(&data, request.data_type) => {
                    debug!(
                        "History provider {} returned batched option {:?} rows for {} tickers ({})",
                        provider_label,
                        request.data_type,
                        request.tickers.len(),
                        request.date
                    );
                    return Ok(data);
                }
                Ok(_) => {
                    debug!(
                        "History provider {} returned 0 batched option {:?} rows for {} tickers ({})",
                        provider_label,
                        request.data_type,
                        request.tickers.len(),
                        request.date
                    );
                    continue;
                }
                Err(ref e) if is_not_implemented(e) => {
                    debug!(
                        "History provider {} does not implement batched option {:?}",
                        provider_label, request.data_type
                    );
                    continue;
                }
                Err(ref e) if is_recoverable_cache_error(e) => {
                    debug!(
                        "History provider {} hit recoverable batched option cache error for {:?}: {}",
                        provider_label, request.data_type, e
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
