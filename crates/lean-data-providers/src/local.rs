/// Local Iceberg history provider — reads cached market data with no network calls.
///
/// Useful as a fallback when data has already been downloaded to the local
/// Parquet store, or in tests.
use chrono::NaiveDate;
use lean_core::{Resolution, Symbol, TickType};
use lean_data::{MarginInterestRate, QuoteBar, Tick, TradeBar};
use lean_storage::{IcebergStore, OptionEodBar, OptionUniverseRow, QueryParams};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use crate::request::HistoryRequest;
use crate::traits::IHistoryProvider;
use async_trait::async_trait;

pub struct LocalHistoryProvider {
    store: Arc<IcebergStore>,
    daily_trade_cache: Mutex<HashMap<u64, Arc<Vec<TradeBar>>>>,
    option_universe_cache: Mutex<Option<OptionUniverseCacheEntry>>,
    /// When true, a daily request that the local store cannot fully satisfy
    /// returns empty (a "miss") instead of best-effort partial rows, so a
    /// stacked remote provider is given a chance to backfill the gap. Enabled
    /// when local sits in front of remote providers; disabled for local-only
    /// runs where partial cached data is better than nothing.
    strict_coverage: bool,
}

struct OptionUniverseCacheEntry {
    date: NaiveDate,
    rows: Arc<Vec<OptionUniverseRow>>,
    loaded_underlyings: HashSet<String>,
}

impl LocalHistoryProvider {
    pub fn new(data_root: impl AsRef<std::path::Path>) -> Self {
        let data_root = data_root.as_ref().to_path_buf();
        let store = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("local history runtime")
                .block_on(IcebergStore::connect_local(data_root))
                .expect("failed to connect local Iceberg history store")
        })
        .join()
        .expect("local history store worker panicked");
        LocalHistoryProvider {
            store: Arc::new(store),
            daily_trade_cache: Mutex::new(HashMap::new()),
            option_universe_cache: Mutex::new(None),
            strict_coverage: false,
        }
    }

    pub fn from_store(store: Arc<IcebergStore>) -> Self {
        LocalHistoryProvider {
            store,
            daily_trade_cache: Mutex::new(HashMap::new()),
            option_universe_cache: Mutex::new(None),
            strict_coverage: false,
        }
    }

    /// Enable strict coverage: incomplete daily windows are reported as a miss
    /// (empty result) so remote providers in the stack can backfill the gap.
    pub fn with_strict_coverage(mut self, strict: bool) -> Self {
        self.strict_coverage = strict;
        self
    }

    async fn cached_daily_trade_rows(&self, symbol: &Symbol) -> anyhow::Result<Arc<Vec<TradeBar>>> {
        let sid = symbol.id.sid;
        if let Some(rows) = self
            .daily_trade_cache
            .lock()
            .expect("local daily trade cache poisoned")
            .get(&sid)
            .cloned()
        {
            return Ok(rows);
        }

        let mut grouped = self
            .store
            .scan_trade_bar_partitions_grouped(
                &HashMap::from([(sid, symbol.clone())]),
                Resolution::Daily,
                TickType::Trade,
                &QueryParams::new().with_symbols(vec![sid]),
            )
            .await?;
        let rows = Arc::new(grouped.remove(&sid).unwrap_or_default());
        let mut guard = self
            .daily_trade_cache
            .lock()
            .expect("local daily trade cache poisoned");
        Ok(guard
            .entry(sid)
            .or_insert_with(|| Arc::clone(&rows))
            .clone())
    }

    fn merge_option_universe_cache(
        &self,
        date: NaiveDate,
        loaded_underlyings: &HashSet<String>,
        rows: &[OptionUniverseRow],
    ) {
        if loaded_underlyings.is_empty() {
            return;
        }

        let mut guard = self
            .option_universe_cache
            .lock()
            .expect("local option universe cache poisoned");
        if let Some(entry) = guard.as_mut().filter(|entry| entry.date == date) {
            let mut merged = entry.rows.as_ref().clone();
            merged
                .retain(|row| !loaded_underlyings.contains(&normalize_underlying(&row.underlying)));
            merged.extend_from_slice(rows);
            entry
                .loaded_underlyings
                .extend(loaded_underlyings.iter().cloned());
            entry.rows = Arc::new(merged);
        } else {
            *guard = Some(OptionUniverseCacheEntry {
                date,
                rows: Arc::new(rows.to_vec()),
                loaded_underlyings: loaded_underlyings.clone(),
            });
        }
    }
}

fn expected_daily_dates(
    symbol: &Symbol,
    start: lean_core::DateTime,
    end: lean_core::DateTime,
) -> HashSet<NaiveDate> {
    let mut dates = HashSet::new();
    let hours = lean_core::MarketHoursDatabase::global().exchange_hours(symbol);
    let mut date = start.date_utc();
    let end_date = end.date_utc();
    while date <= end_date {
        if date
            .and_hms_opt(12, 0, 0)
            .map(|midday| hours.is_open_at_local_naive(midday))
            .unwrap_or(false)
        {
            dates.insert(date);
        }
        date += chrono::Duration::days(1);
    }
    dates
}

fn timestamp_in_range(
    timestamp: lean_core::DateTime,
    start: lean_core::DateTime,
    end: lean_core::DateTime,
) -> bool {
    timestamp >= start && timestamp <= end
}

fn daily_bar_overlaps_range(
    row: &TradeBar,
    start: lean_core::DateTime,
    end: lean_core::DateTime,
) -> bool {
    timestamp_in_range(row.time, start, end) || timestamp_in_range(row.end_time, start, end)
}

fn has_complete_daily_coverage(
    symbol: &Symbol,
    rows: &[TradeBar],
    start: lean_core::DateTime,
    end: lean_core::DateTime,
) -> bool {
    let expected = expected_daily_dates(symbol, start, end);
    if expected.is_empty() {
        return rows.is_empty();
    }
    let mut available = HashSet::new();
    let mut unique_rows = HashSet::new();
    for row in rows {
        let time_date = row.time.date_utc();
        let end_date = row.end_time.date_utc();
        if expected.contains(&time_date) {
            available.insert(time_date);
            unique_rows.insert((row.symbol.id.sid, row.time.0, row.end_time.0));
        }
        if expected.contains(&end_date) {
            available.insert(end_date);
            unique_rows.insert((row.symbol.id.sid, row.time.0, row.end_time.0));
        }
    }
    expected.is_subset(&available) && unique_rows.len() >= expected.len()
}

#[async_trait]
impl IHistoryProvider for LocalHistoryProvider {
    fn name(&self) -> &str {
        "local"
    }

    async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
        use crate::request::DataType;
        // LocalHistoryProvider only serves trade bars from disk.
        // Any other DataType (FactorFile, etc.) must go to a remote provider.
        if request.data_type != DataType::TradeBar {
            return Err(anyhow::anyhow!(
                "NotImplemented: LocalHistoryProvider does not handle {:?}",
                request.data_type
            ));
        }
        let symbol = request.symbol.clone();
        let sid = symbol.id.sid;
        let mut rows = if request.resolution == Resolution::Daily {
            self.cached_daily_trade_rows(&symbol)
                .await?
                .as_ref()
                .clone()
        } else {
            let params = QueryParams::new()
                .with_day_range(request.start, request.end)
                .with_bar_range(request.start, request.end)
                .with_symbols(vec![sid]);
            let mut grouped = self
                .store
                .scan_trade_bar_partitions_grouped(
                    &HashMap::from([(sid, symbol)]),
                    request.resolution,
                    TickType::Trade,
                    &params,
                )
                .await?;
            grouped.remove(&sid).unwrap_or_default()
        };
        if request.resolution == Resolution::Daily {
            rows.retain(|row| daily_bar_overlaps_range(row, request.start, request.end));
            if rows.is_empty() {
                return Ok(Vec::new());
            }
            if !has_complete_daily_coverage(&request.symbol, &rows, request.start, request.end) {
                if self.strict_coverage {
                    tracing::debug!(
                        symbol = %request.symbol.value,
                        start = %request.start,
                        end = %request.end,
                        rows = rows.len(),
                        "local daily coverage incomplete; reporting miss so remote can backfill"
                    );
                    return Ok(Vec::new());
                }
                tracing::debug!(
                    symbol = %request.symbol.value,
                    start = %request.start,
                    end = %request.end,
                    rows = rows.len(),
                    "returning best-effort local daily history"
                );
            }
        }
        Ok(rows)
    }

    async fn get_quote_bars(&self, request: &HistoryRequest) -> anyhow::Result<Vec<QuoteBar>> {
        let symbol = request.symbol.clone();
        let sid = symbol.id.sid;
        let params = QueryParams::new()
            .with_day_range(request.start, request.end)
            .with_bar_range(request.start, request.end)
            .with_symbols(vec![sid]);
        let mut grouped = self
            .store
            .scan_quote_bar_partitions_grouped(
                &HashMap::from([(sid, symbol)]),
                request.resolution,
                TickType::Quote,
                &params,
            )
            .await?;
        Ok(grouped.remove(&sid).unwrap_or_default())
    }

    async fn get_ticks(&self, request: &HistoryRequest) -> anyhow::Result<Vec<Tick>> {
        let symbol = request.symbol.clone();
        let sid = symbol.id.sid;
        let params = QueryParams::new()
            .with_time_range(request.start, request.end)
            .with_symbols(vec![sid]);
        let mut grouped = self
            .store
            .scan_tick_partitions_grouped(&HashMap::from([(sid, symbol)]), &params)
            .await?;
        Ok(grouped.remove(&sid).unwrap_or_default())
    }

    async fn get_margin_interest_rates(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<MarginInterestRate>> {
        let mut params = QueryParams::new().with_time_range(request.start, request.end);
        params.predicate = params.predicate.with_symbols(vec![request.symbol.id.sid]);
        Ok(self
            .store
            .scan_margin_interest_rates(&request.symbol, &params)
            .await?)
    }

    async fn get_perpetual_contexts(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<lean_data::PerpetualContext>> {
        let mut params = QueryParams::new().with_time_range(request.start, request.end);
        params.predicate = params.predicate.with_symbols(vec![request.symbol.id.sid]);
        Ok(self
            .store
            .scan_perpetual_contexts(&request.symbol, &params)
            .await?)
    }

    async fn get_option_eod_bars(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<OptionEodBar>> {
        let ticker_key = normalize_underlying(ticker);
        self.store
            .scan_option_eod_bars(std::slice::from_ref(&ticker_key), date)
            .await
    }

    async fn get_option_universe(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_storage::OptionUniverseRow>> {
        let ticker_key = normalize_underlying(ticker);
        if let Some(rows) = self
            .option_universe_cache
            .lock()
            .expect("local option universe cache poisoned")
            .as_ref()
            .filter(|entry| entry.date == date && entry.loaded_underlyings.contains(&ticker_key))
            .map(|entry| Arc::clone(&entry.rows))
        {
            return Ok(filter_option_universe_rows(&rows, &ticker_key));
        }

        let rows = self
            .store
            .scan_option_universe(std::slice::from_ref(&ticker_key), date)
            .await?;
        self.merge_option_universe_cache(date, &HashSet::from([ticker_key.clone()]), &rows);

        Ok(filter_option_universe_rows(&rows, &ticker_key))
    }

    async fn get_option_universes(
        &self,
        tickers: &[String],
        date: chrono::NaiveDate,
    ) -> anyhow::Result<HashMap<String, Vec<lean_storage::OptionUniverseRow>>> {
        let requested = normalized_underlyings(tickers);
        if requested.is_empty() {
            return Ok(HashMap::new());
        }

        let mut out = HashMap::new();
        let mut missing = Vec::new();
        if let Some(rows) = self
            .option_universe_cache
            .lock()
            .expect("local option universe cache poisoned")
            .as_ref()
            .filter(|entry| entry.date == date)
            .map(|entry| (Arc::clone(&entry.rows), entry.loaded_underlyings.clone()))
        {
            for ticker in &requested {
                if rows.1.contains(ticker) {
                    out.insert(ticker.clone(), filter_option_universe_rows(&rows.0, ticker));
                } else {
                    missing.push(ticker.clone());
                }
            }
        } else {
            missing.extend(requested.iter().cloned());
        }

        if missing.is_empty() {
            return Ok(out);
        }

        let loaded_rows = self.store.scan_option_universe(&missing, date).await?;
        let loaded_underlyings = missing.iter().cloned().collect::<HashSet<_>>();
        self.merge_option_universe_cache(date, &loaded_underlyings, &loaded_rows);

        for ticker in missing {
            out.insert(
                ticker.clone(),
                filter_option_universe_rows(&loaded_rows, &ticker),
            );
        }

        Ok(out)
    }
}

fn normalize_underlying(ticker: &str) -> String {
    ticker.trim().trim_start_matches('?').to_ascii_uppercase()
}

fn normalized_underlyings(tickers: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for ticker in tickers {
        let normalized = normalize_underlying(ticker);
        if normalized.is_empty() || !seen.insert(normalized.clone()) {
            continue;
        }
        out.push(normalized);
    }
    out
}

fn filter_option_universe_rows(rows: &[OptionUniverseRow], ticker: &str) -> Vec<OptionUniverseRow> {
    rows.iter()
        .filter(|row| row.underlying.eq_ignore_ascii_case(ticker))
        .cloned()
        .collect()
}
