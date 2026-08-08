use std::sync::atomic::{AtomicU64, Ordering};
use std::{collections::VecDeque, sync::Arc};

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::NaiveDate;
use futures::{stream, StreamExt, TryStreamExt};
use rlean_core::{
    DateTime, Market, NanosecondTimestamp, OptionRight, OptionStyle, SecurityIdentifier,
    SecurityType, Symbol,
};
use rlean_data::{SubscriptionDataConfig, SubscriptionDataKind};
use rlean_data_tables::{
    CustomDataPoint, FactorFileEntry, FundamentalUniverseRow, FutureUniverseRow, MapFileEntry,
    OptionUniverseRow, QuoteBar, RiskFreeInterestRate, Tick, TradeBar,
};

/// A half-open UTC range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub start: DateTime,
    pub end: DateTime,
}

impl TimeRange {
    pub fn new(start: DateTime, end: DateTime) -> Result<Self> {
        if start >= end {
            bail!("history range start must be before end");
        }
        Ok(Self { start, end })
    }
}

/// Provider-neutral history request. `SubscriptionDataConfig` owns the same
/// symbol/type/resolution intent used by an algorithm subscription.
#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub configuration: SubscriptionDataConfig,
    pub range: TimeRange,
}

impl HistoryRequest {
    pub fn new(
        configuration: SubscriptionDataConfig,
        start: DateTime,
        end: DateTime,
    ) -> Result<Self> {
        Ok(Self {
            configuration,
            range: TimeRange::new(start, end)?,
        })
    }

    pub fn with_range(&self, range: TimeRange) -> Self {
        Self {
            configuration: self.configuration.clone(),
            range,
        }
    }
}

/// Canonical typed values returned by every history provider and store.
#[derive(Debug, Clone)]
pub enum HistoricalData {
    TradeBars(Vec<TradeBar>),
    QuoteBars(Vec<QuoteBar>),
    Ticks(Vec<Tick>),
    CustomPoints(Vec<CustomDataPoint>),
    OptionUniverse(Vec<OptionUniverseRow>),
    FutureUniverse(Vec<FutureUniverseRow>),
    FundamentalUniverse(Vec<FundamentalUniverseRow>),
}

impl HistoricalData {
    pub fn is_empty(&self) -> bool {
        match self {
            Self::TradeBars(rows) => rows.is_empty(),
            Self::QuoteBars(rows) => rows.is_empty(),
            Self::Ticks(rows) => rows.is_empty(),
            Self::CustomPoints(rows) => rows.is_empty(),
            Self::OptionUniverse(rows) => rows.is_empty(),
            Self::FutureUniverse(rows) => rows.is_empty(),
            Self::FundamentalUniverse(rows) => rows.is_empty(),
        }
    }

    /// Human-readable row family, used only for merge diagnostics.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TradeBars(_) => "trade bar",
            Self::QuoteBars(_) => "quote bar",
            Self::Ticks(_) => "tick",
            Self::CustomPoints(_) => "custom point",
            Self::OptionUniverse(_) => "option universe",
            Self::FutureUniverse(_) => "future universe",
            Self::FundamentalUniverse(_) => "fundamental universe",
        }
    }

    /// Merge rows of the same family. Ordering and duplicate removal stay with
    /// `sort_and_deduplicate`, which the caller runs over the merged result.
    pub fn extend(&mut self, other: Self) -> Result<()> {
        match (self, other) {
            (Self::TradeBars(rows), Self::TradeBars(other)) => rows.extend(other),
            (Self::QuoteBars(rows), Self::QuoteBars(other)) => rows.extend(other),
            (Self::Ticks(rows), Self::Ticks(other)) => rows.extend(other),
            (Self::CustomPoints(rows), Self::CustomPoints(other)) => rows.extend(other),
            (Self::OptionUniverse(rows), Self::OptionUniverse(other)) => rows.extend(other),
            (Self::FutureUniverse(rows), Self::FutureUniverse(other)) => rows.extend(other),
            (Self::FundamentalUniverse(rows), Self::FundamentalUniverse(other)) => {
                rows.extend(other)
            }
            (left, right) => bail!(
                "cannot merge {} history rows into {} history rows",
                right.kind(),
                left.kind()
            ),
        }
        Ok(())
    }

    pub fn sort_and_deduplicate(&mut self) {
        match self {
            Self::TradeBars(rows) => {
                rows.sort_by_key(|row| row.end_time);
                rows.dedup_by(|left, right| {
                    left.symbol == right.symbol
                        && left.venue == right.venue
                        && left.time == right.time
                        && left.end_time == right.end_time
                });
            }
            Self::QuoteBars(rows) => {
                rows.sort_by_key(|row| row.end_time);
                rows.dedup_by(|left, right| {
                    left.symbol == right.symbol
                        && left.venue == right.venue
                        && left.time == right.time
                        && left.end_time == right.end_time
                });
            }
            Self::Ticks(rows) => {
                rows.sort_by_key(|row| row.time);
                rows.dedup_by(|left, right| {
                    left.symbol == right.symbol
                        && left.venue == right.venue
                        && left.time == right.time
                        && left.tick_type == right.tick_type
                        && left.exchange == right.exchange
                        && left.value == right.value
                        && left.quantity == right.quantity
                });
            }
            // LEAN custom data may contain several independent records at the
            // same EndTime. Preserve all of them and only establish the
            // monotonic ordering expected by SubscriptionDataReader.
            Self::CustomPoints(rows) => rows.sort_by_key(|row| (row.end_time, row.time)),
            Self::OptionUniverse(rows) => {
                rows.sort_by(|left, right| {
                    (left.date, &left.symbol_sid).cmp(&(right.date, &right.symbol_sid))
                });
                rows.dedup_by(|left, right| {
                    left.date == right.date && left.symbol_sid == right.symbol_sid
                });
            }
            Self::FutureUniverse(rows) => {
                rows.sort_by(|left, right| {
                    (left.date, &left.symbol_sid).cmp(&(right.date, &right.symbol_sid))
                });
                rows.dedup_by(|left, right| {
                    left.date == right.date && left.symbol_sid == right.symbol_sid
                });
            }
            Self::FundamentalUniverse(rows) => {
                rows.sort_by_key(|row| (row.end_time, row.symbol_sid));
                rows.dedup_by(|left, right| {
                    left.end_time == right.end_time && left.symbol_sid == right.symbol_sid
                });
            }
        }
    }
}

/// Coverage is recorded separately from rows so a successful empty provider
/// response is durable. Errors never advance coverage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    pub covered: Vec<TimeRange>,
}

impl Coverage {
    pub fn missing(&self, requested: TimeRange) -> Vec<TimeRange> {
        let mut covered = self.covered.clone();
        covered.sort_by_key(|range| range.start);
        let mut cursor = requested.start;
        let mut missing = Vec::new();
        for range in covered {
            if range.end <= cursor || range.start >= requested.end {
                continue;
            }
            let start = range.start.max(requested.start);
            let end = range.end.min(requested.end);
            if start > cursor {
                missing.push(TimeRange {
                    start: cursor,
                    end: start,
                });
            }
            cursor = cursor.max(end);
            if cursor >= requested.end {
                break;
            }
        }
        if cursor < requested.end {
            missing.push(TimeRange {
                start: cursor,
                end: requested.end,
            });
        }
        missing
    }
}

/// A rate publisher or the canonical cache could not be reached.
///
/// Availability is the only failure a caller may degrade past: a deployment
/// with no reachable publisher still has LEAN's default rate. Correctness
/// failures — malformed vendor rows, refused canonical writes — carry no such
/// marker and stay fatal, because a wrong rate silently corrupts every
/// risk-adjusted statistic in the run.
#[derive(Debug)]
pub struct RiskFreeInterestRateUnavailable(String);

impl RiskFreeInterestRateUnavailable {
    pub fn error(message: impl Into<String>) -> anyhow::Error {
        anyhow::Error::new(Self(message.into()))
    }
}

impl std::fmt::Display for RiskFreeInterestRateUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RiskFreeInterestRateUnavailable {}

#[async_trait]
pub trait HistoricalDataProvider: Send + Sync {
    fn name(&self) -> &str;
    fn supports(&self, request: &HistoryRequest) -> bool;
    async fn get_history(&self, request: &HistoryRequest) -> Result<HistoricalData>;
    /// Opportunistically warm canonical cached data for subscriptions the
    /// engine has already selected. Providers without a cache need no action.
    async fn prefetch_history(&self, _requests: &[HistoryRequest]) -> Result<()> {
        Ok(())
    }
    /// LEAN `IFactorFileProvider` boundary. `None` means unsupported; an empty
    /// vector is a successful provider response.
    async fn get_factor_file(&self, _symbol: &Symbol) -> Result<Option<Vec<FactorFileEntry>>> {
        Ok(None)
    }
    /// LEAN `IMapFileProvider` boundary.
    async fn get_map_file(&self, _symbol: &Symbol) -> Result<Option<Vec<MapFileEntry>>> {
        Ok(None)
    }
    /// LEAN `IRiskFreeInterestRateModel` source. The series is global rather
    /// than per-symbol, so it has no `HistoryRequest`, exactly like the factor
    /// and map files above. `None` means the provider publishes no rates; an
    /// empty vector is a successful provider response.
    async fn get_risk_free_interest_rates(
        &self,
        _range: TimeRange,
    ) -> Result<Option<Vec<RiskFreeInterestRate>>> {
        Ok(None)
    }
}

/// Result of persisting a canonical batch to the cache.
///
/// Persisting is an optimization for later runs: the rows are already in
/// memory feeding the engine. A cache backend that will not commit therefore
/// drops the batch instead of unwinding the run, and the caller keeps its own
/// copy of the rows and leaves the range uncovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheAppendOutcome {
    Persisted,
    Dropped,
}

/// Per-run tally of canonical batches the cache refused to commit. A run that
/// silently lost half its cache writes must not look like a clean run, so the
/// count is reported once when the run finishes.
#[derive(Clone, Default)]
pub struct DroppedCacheWrites {
    inner: Arc<DroppedCacheWritesCounters>,
}

#[derive(Default)]
struct DroppedCacheWritesCounters {
    batches: AtomicU64,
    rows: AtomicU64,
}

impl DroppedCacheWrites {
    pub fn record(&self, rows: usize) {
        self.inner.batches.fetch_add(1, Ordering::Relaxed);
        self.inner.rows.fetch_add(rows as u64, Ordering::Relaxed);
    }

    pub fn batches(&self) -> u64 {
        self.inner.batches.load(Ordering::Relaxed)
    }

    pub fn rows(&self) -> u64 {
        self.inner.rows.load(Ordering::Relaxed)
    }

    /// One summary line at the end of a run. Silence means nothing was lost.
    pub fn log_summary(&self) {
        let batches = self.batches();
        if batches == 0 {
            return;
        }
        tracing::error!(
            dropped_batches = batches,
            dropped_rows = self.rows(),
            "canonical history cache writes were dropped during this run; the affected ranges \
             stay uncovered and a later run refetches and repersists them"
        );
    }
}

/// Verglas implements this boundary. The provider coordinator depends only on
/// canonical data and coverage, so local/remote cache placement is invisible.
#[async_trait]
pub trait HistoricalDataStore: Send + Sync {
    async fn coverage(&self, request: &HistoryRequest) -> Result<Coverage>;
    async fn read(&self, request: &HistoryRequest) -> Result<HistoricalData>;
    async fn append(
        &self,
        request: &HistoryRequest,
        provider: &str,
        data: &HistoricalData,
    ) -> Result<CacheAppendOutcome>;
    async fn mark_covered(&self, request: &HistoryRequest, provider: &str) -> Result<()>;
    async fn prefetch(&self, _requests: &[HistoryRequest]) -> Result<()> {
        Ok(())
    }
    async fn read_factor_file(&self, _symbol: &Symbol) -> Result<Vec<FactorFileEntry>> {
        Ok(Vec::new())
    }
    async fn append_factor_file(
        &self,
        _symbol: &Symbol,
        _provider: &str,
        _rows: &[FactorFileEntry],
    ) -> Result<()> {
        Ok(())
    }
    async fn read_map_file(&self, _symbol: &Symbol) -> Result<Vec<MapFileEntry>> {
        Ok(Vec::new())
    }
    async fn append_map_file(
        &self,
        _symbol: &Symbol,
        _provider: &str,
        _rows: &[MapFileEntry],
    ) -> Result<()> {
        Ok(())
    }
    async fn read_risk_free_interest_rates(
        &self,
        _range: TimeRange,
    ) -> Result<Vec<RiskFreeInterestRate>> {
        Ok(Vec::new())
    }
    async fn append_risk_free_interest_rates(
        &self,
        _provider: &str,
        _rows: &[RiskFreeInterestRate],
    ) -> Result<()> {
        Ok(())
    }
}

/// LEAN-style cache/provider composition: read persisted data first, request
/// only uncovered spans, persist successful results, then read one canonical
/// ordered result from the store. This also prevents provider overlap from
/// leaking duplicate bars into the subscription enumerator.
pub struct CacheFirstHistoryProvider {
    store: Arc<dyn HistoricalDataStore>,
    providers: Vec<Arc<dyn HistoricalDataProvider>>,
    prefetched_coverage: tokio::sync::Mutex<VecDeque<PrefetchedCoverage>>,
}

const PREFETCHED_COVERAGE_CAPACITY: usize = 8_192;

#[derive(Clone)]
struct PrefetchedCoverage {
    subscription_id: u64,
    range: TimeRange,
    coverage: Coverage,
}

impl CacheFirstHistoryProvider {
    pub fn new(
        store: Arc<dyn HistoricalDataStore>,
        providers: Vec<Arc<dyn HistoricalDataProvider>>,
    ) -> Result<Self> {
        if providers.is_empty() {
            bail!("at least one historical data provider is required");
        }
        Ok(Self {
            store,
            providers,
            prefetched_coverage: tokio::sync::Mutex::new(VecDeque::new()),
        })
    }

    async fn prefetched_coverage(&self, request: &HistoryRequest) -> Option<Coverage> {
        let subscription_id = request.configuration.unique_id();
        self.prefetched_coverage
            .lock()
            .await
            .iter()
            .rev()
            .find(|entry| {
                entry.subscription_id == subscription_id
                    && entry.range.start <= request.range.start
                    && entry.range.end >= request.range.end
            })
            .map(|entry| entry.coverage.clone())
    }

    async fn remember_prefetched_coverage(&self, request: &HistoryRequest, coverage: Coverage) {
        let mut cache = self.prefetched_coverage.lock().await;
        cache.push_back(PrefetchedCoverage {
            subscription_id: request.configuration.unique_id(),
            range: request.range,
            coverage,
        });
        while cache.len() > PREFETCHED_COVERAGE_CAPACITY {
            cache.pop_front();
        }
    }

    async fn canonicalize_option_universe(
        &self,
        request: &HistoryRequest,
        data: &mut HistoricalData,
    ) -> Result<()> {
        let HistoricalData::OptionUniverse(rows) = data else {
            return Ok(());
        };
        let metadata =
            request.configuration.option_chain.as_ref().ok_or_else(|| {
                anyhow::anyhow!("option-universe request is missing chain metadata")
            })?;
        let underlying = request
            .configuration
            .symbol
            .underlying
            .as_deref()
            .cloned()
            .unwrap_or_else(|| Symbol::create_equity(&metadata.underlying_ticker, &Market::usa()));
        let mut map_rows = self.store.read_map_file(&underlying).await?;
        if map_rows.is_empty() {
            for provider in &self.providers {
                if let Some(rows) = provider.get_map_file(&underlying).await? {
                    if !rows.is_empty() {
                        self.store
                            .append_map_file(&underlying, provider.name(), &rows)
                            .await?;
                    }
                    map_rows = rows;
                    break;
                }
            }
        }
        map_rows.sort_by_key(|row| row.date);
        let (first_ticker, first_date) = map_rows
            .first()
            .map(|row| (row.mapped_symbol.as_str(), row.date))
            .unwrap_or((metadata.underlying_ticker.as_str(), lean_default_sid_date()));
        canonicalize_option_universe_rows(
            rows,
            first_ticker,
            first_date,
            underlying.market(),
            underlying.security_type(),
        )
    }
}

fn lean_default_sid_date() -> NaiveDate {
    NaiveDate::from_ymd_opt(1899, 12, 30).expect("valid LEAN default SID date")
}

fn canonicalize_option_universe_rows(
    rows: &mut [OptionUniverseRow],
    first_ticker: &str,
    first_date: NaiveDate,
    market: &Market,
    underlying_security_type: SecurityType,
) -> Result<()> {
    let underlying_sid = SecurityIdentifier::lean_security_sid(
        first_ticker,
        market,
        underlying_security_type,
        first_date,
    )?;
    for row in rows {
        let Some(expiry) = row.expiration else {
            row.symbol_sid.clone_from(&underlying_sid);
            row.underlying_sid = None;
            continue;
        };
        let strike = row
            .strike
            .ok_or_else(|| anyhow::anyhow!("option-universe contract is missing its strike"))?;
        let right = match row.right.as_deref().map(str::trim) {
            Some(value)
                if value.eq_ignore_ascii_case("call") || value.eq_ignore_ascii_case("c") =>
            {
                OptionRight::Call
            }
            Some(value) if value.eq_ignore_ascii_case("put") || value.eq_ignore_ascii_case("p") => {
                OptionRight::Put
            }
            _ => bail!("option-universe contract has an invalid right"),
        };
        let style = if underlying_security_type == SecurityType::Index {
            OptionStyle::European
        } else {
            OptionStyle::American
        };
        row.symbol_sid = SecurityIdentifier::lean_option_sid(
            first_ticker,
            market,
            underlying_security_type,
            first_date,
            expiry,
            strike,
            right,
            style,
        )?;
        row.underlying_sid = Some(underlying_sid.clone());
    }
    Ok(())
}

#[async_trait]
impl HistoricalDataProvider for CacheFirstHistoryProvider {
    fn name(&self) -> &str {
        "cache-first"
    }

    fn supports(&self, request: &HistoryRequest) -> bool {
        request.configuration.data_kind == SubscriptionDataKind::Custom
            || self
                .providers
                .iter()
                .any(|provider| provider.supports(request))
    }

    async fn get_history(&self, request: &HistoryRequest) -> Result<HistoricalData> {
        // Custom data is produced and persisted by operator applications. It
        // has no native market-data fallback: LEAN distinguishes custom
        // subscriptions through SubscriptionDataConfig.IsCustomData and reads
        // their declared source directly instead of asking the equity provider.
        if request.configuration.data_kind == SubscriptionDataKind::Custom {
            let mut data = self.store.read(request).await?;
            data.sort_and_deduplicate();
            return Ok(data);
        }
        let provider = self
            .providers
            .iter()
            .find(|provider| provider.supports(request))
            .ok_or_else(|| anyhow::anyhow!("no historical provider supports the request"))?;
        let coverage = match self.prefetched_coverage(request).await {
            Some(coverage) => coverage,
            None => self.store.coverage(request).await?,
        };
        let mut updated_coverage = coverage.clone();
        let mut unpersisted = Vec::new();
        for missing in coverage.missing(request.range) {
            let missing_request = request.with_range(missing);
            // Mark coverage only after a successful response. Empty success is
            // intentional negative knowledge and must survive restarts.
            let mut fetched = provider.get_history(&missing_request).await?;
            self.canonicalize_option_universe(&missing_request, &mut fetched)
                .await?;
            fetched.sort_and_deduplicate();
            let outcome = if fetched.is_empty() {
                CacheAppendOutcome::Persisted
            } else {
                self.store
                    .append(&missing_request, provider.name(), &fetched)
                    .await?
            };
            // A dropped cache batch keeps this run correct from the rows the
            // provider already returned, but the span stays uncovered in both
            // the durable coverage table and this run's coverage view so a
            // later run refetches and repersists it.
            if outcome == CacheAppendOutcome::Dropped {
                unpersisted.push(fetched);
                continue;
            }
            self.store
                .mark_covered(&missing_request, provider.name())
                .await?;
            updated_coverage.covered.push(missing);
        }
        self.remember_prefetched_coverage(request, updated_coverage)
            .await;
        let mut data = self.store.read(request).await?;
        // The store cannot serve what it did not commit. Rows whose append was
        // dropped are merged back from memory; a partially committed batch
        // duplicates instead of disappearing, and dedup below removes it.
        for retained in unpersisted {
            data.extend(retained)?;
        }
        data.sort_and_deduplicate();
        Ok(data)
    }

    async fn prefetch_history(&self, requests: &[HistoryRequest]) -> Result<()> {
        let coverage = stream::iter(requests.iter().cloned())
            .map(|request| async move {
                let coverage = self.store.coverage(&request).await?;
                Ok::<_, anyhow::Error>((request, coverage))
            })
            .buffer_unordered(512)
            .try_collect::<Vec<_>>()
            .await?;
        for (request, coverage) in coverage {
            // Retain partial durable coverage too. A short-lived contract can
            // only be covered for its own session inside a wider universe
            // prefetch window; its later concrete request is a covered subset.
            self.remember_prefetched_coverage(&request, coverage).await;
        }
        self.store.prefetch(requests).await
    }

    async fn get_factor_file(&self, symbol: &Symbol) -> Result<Option<Vec<FactorFileEntry>>> {
        let cached = self.store.read_factor_file(symbol).await?;
        if !cached.is_empty() {
            return Ok(Some(cached));
        }
        for provider in &self.providers {
            if let Some(rows) = provider.get_factor_file(symbol).await? {
                if !rows.is_empty() {
                    self.store
                        .append_factor_file(symbol, provider.name(), &rows)
                        .await?;
                }
                return Ok(Some(rows));
            }
        }
        Ok(None)
    }

    async fn get_map_file(&self, symbol: &Symbol) -> Result<Option<Vec<MapFileEntry>>> {
        let cached = self.store.read_map_file(symbol).await?;
        if !cached.is_empty() {
            return Ok(Some(cached));
        }
        for provider in &self.providers {
            if let Some(rows) = provider.get_map_file(symbol).await? {
                if !rows.is_empty() {
                    self.store
                        .append_map_file(symbol, provider.name(), &rows)
                        .await?;
                }
                return Ok(Some(rows));
            }
        }
        Ok(None)
    }

    async fn get_risk_free_interest_rates(
        &self,
        range: TimeRange,
    ) -> Result<Option<Vec<RiskFreeInterestRate>>> {
        let mut rows = self
            .store
            .read_risk_free_interest_rates(range)
            .await
            .map_err(|error| {
                RiskFreeInterestRateUnavailable::error(format!(
                    "read cached risk-free interest rates: {error:#}"
                ))
            })?;
        let mut published = !rows.is_empty();
        // Append-only + forward-filled: interior gaps between observations are
        // non-publication days (weekends/holidays), not uncovered cache holes.
        // The spans that can still be missing are the prefix before the
        // earliest cached row and the suffix after the newest — matching how
        // LEAN's InterestRateProvider expects the full CSV history for the
        // requested window before forward-filling.
        for missing in risk_free_rate_missing_ranges(range, &rows) {
            for provider in &self.providers {
                let Some(fetched) = provider.get_risk_free_interest_rates(missing).await? else {
                    continue;
                };
                published = true;
                if fetched.is_empty() {
                    continue;
                }
                self.store
                    .append_risk_free_interest_rates(provider.name(), &fetched)
                    .await?;
                rows.extend(fetched);
            }
        }
        if !published {
            return Ok(None);
        }
        rows.sort_by_key(|row| row.time);
        rows.dedup_by(|left, right| left.time == right.time && left.venue == right.venue);
        Ok(Some(rows))
    }
}

/// Uncovered spans for a cached risk-free series inside `requested`.
///
/// Empty cache → the whole range. Otherwise only the prefix before the earliest
/// row and the suffix after the newest row; interior calendar gaps stay local
/// to the forward-fill model.
fn risk_free_rate_missing_ranges(
    requested: TimeRange,
    rows: &[RiskFreeInterestRate],
) -> Vec<TimeRange> {
    let Some(earliest) = rows.iter().map(|row| row.time).min() else {
        return vec![requested];
    };
    let latest = rows
        .iter()
        .map(|row| row.time)
        .max()
        .expect("earliest implies latest");
    let mut missing = Vec::new();
    if earliest > requested.start {
        if let Ok(prefix) = TimeRange::new(requested.start, earliest) {
            missing.push(prefix);
        }
    }
    if let Ok(suffix) = TimeRange::new(NanosecondTimestamp(latest.0 + 1), requested.end) {
        missing.push(suffix);
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use rlean_core::{
        DataNormalizationMode, Market, NanosecondTimestamp, Resolution, Symbol, TimeSpan,
    };
    use rlean_data::{CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata};
    use rust_decimal::Decimal;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct EmptyProvider {
        calls: AtomicUsize,
        fail: bool,
    }

    #[async_trait]
    impl HistoricalDataProvider for EmptyProvider {
        fn name(&self) -> &str {
            "empty"
        }

        fn supports(&self, _request: &HistoryRequest) -> bool {
            true
        }

        async fn get_history(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                bail!("provider failed");
            }
            Ok(HistoricalData::TradeBars(Vec::new()))
        }
    }

    #[derive(Default)]
    struct MemoryStore {
        coverage: Mutex<Coverage>,
        coverage_calls: AtomicUsize,
        marks: AtomicUsize,
        reads: AtomicUsize,
        /// Rows the store actually committed. A dropped append never lands
        /// here, which is what makes the read-back path observable.
        persisted: Mutex<Vec<TradeBar>>,
        rates: Mutex<Vec<RiskFreeInterestRate>>,
        drop_appends: bool,
    }

    #[async_trait]
    impl HistoricalDataStore for MemoryStore {
        async fn coverage(&self, _request: &HistoryRequest) -> Result<Coverage> {
            self.coverage_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self.coverage.lock().clone())
        }

        async fn read(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(HistoricalData::TradeBars(self.persisted.lock().clone()))
        }

        async fn append(
            &self,
            _request: &HistoryRequest,
            _provider: &str,
            data: &HistoricalData,
        ) -> Result<CacheAppendOutcome> {
            if self.drop_appends {
                return Ok(CacheAppendOutcome::Dropped);
            }
            if let HistoricalData::TradeBars(rows) = data {
                self.persisted.lock().extend(rows.iter().cloned());
            }
            Ok(CacheAppendOutcome::Persisted)
        }

        async fn mark_covered(&self, request: &HistoryRequest, _provider: &str) -> Result<()> {
            self.marks.fetch_add(1, Ordering::Relaxed);
            self.coverage.lock().covered.push(request.range);
            Ok(())
        }

        async fn read_risk_free_interest_rates(
            &self,
            range: TimeRange,
        ) -> Result<Vec<RiskFreeInterestRate>> {
            Ok(self
                .rates
                .lock()
                .iter()
                .filter(|row| row.time >= range.start && row.time < range.end)
                .cloned()
                .collect())
        }

        async fn append_risk_free_interest_rates(
            &self,
            _provider: &str,
            rows: &[RiskFreeInterestRate],
        ) -> Result<()> {
            self.rates.lock().extend(rows.iter().cloned());
            Ok(())
        }
    }

    struct BarProvider {
        calls: AtomicUsize,
        bar: TradeBar,
    }

    #[async_trait]
    impl HistoricalDataProvider for BarProvider {
        fn name(&self) -> &str {
            "bars"
        }

        fn supports(&self, _request: &HistoryRequest) -> bool {
            true
        }

        async fn get_history(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(HistoricalData::TradeBars(vec![self.bar.clone()]))
        }
    }

    fn bar_provider() -> Arc<BarProvider> {
        Arc::new(BarProvider {
            calls: AtomicUsize::new(0),
            bar: TradeBar {
                symbol: Symbol::create_equity("SPY", &Market::usa()),
                venue: Some("usa".to_owned()),
                time: NanosecondTimestamp(10),
                end_time: NanosecondTimestamp(20),
                open: Decimal::new(41100, 2),
                high: Decimal::new(41200, 2),
                low: Decimal::new(41000, 2),
                close: Decimal::new(41150, 2),
                volume: Decimal::new(1234, 0),
                period: TimeSpan::ONE_DAY,
            },
        })
    }

    fn request() -> HistoryRequest {
        HistoryRequest::new(
            SubscriptionDataConfig::new_equity(
                Symbol::create_equity("SPY", &Market::usa()),
                Resolution::Daily,
                DataNormalizationMode::Raw,
            ),
            NanosecondTimestamp(0),
            NanosecondTimestamp(100),
        )
        .unwrap()
    }

    fn custom_request() -> HistoryRequest {
        let source_type = "unusual_whales".to_string();
        let ticker = "market_tide".to_string();
        let query = CustomDataQuery::default();
        let metadata = CustomSubscriptionMetadata {
            source_type: source_type.clone(),
            ticker: ticker.clone(),
            config: CustomDataConfig {
                ticker: ticker.clone(),
                source_type: source_type.clone(),
                resolution: Resolution::Minute,
                properties: HashMap::new(),
                query: query.clone(),
            },
            dynamic_query: query,
        };
        HistoryRequest::new(
            SubscriptionDataConfig::new_custom(
                Symbol::create_base(&source_type, &ticker, &Market::usa()),
                Resolution::Minute,
                metadata,
            ),
            NanosecondTimestamp(0),
            NanosecondTimestamp(100),
        )
        .unwrap()
    }

    #[test]
    fn coverage_returns_only_uncovered_ranges() {
        let requested = TimeRange {
            start: NanosecondTimestamp(0),
            end: NanosecondTimestamp(100),
        };
        let coverage = Coverage {
            covered: vec![
                TimeRange {
                    start: NanosecondTimestamp(20),
                    end: NanosecondTimestamp(40),
                },
                TimeRange {
                    start: NanosecondTimestamp(60),
                    end: NanosecondTimestamp(80),
                },
            ],
        };
        assert_eq!(
            coverage.missing(requested),
            vec![
                TimeRange {
                    start: NanosecondTimestamp(0),
                    end: NanosecondTimestamp(20)
                },
                TimeRange {
                    start: NanosecondTimestamp(40),
                    end: NanosecondTimestamp(60)
                },
                TimeRange {
                    start: NanosecondTimestamp(80),
                    end: NanosecondTimestamp(100)
                }
            ]
        );
    }

    #[tokio::test]
    async fn successful_empty_response_is_covered_and_not_requested_twice() {
        let provider = Arc::new(EmptyProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();
        coordinator.get_history(&request()).await.unwrap();
        coordinator.get_history(&request()).await.unwrap();
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        assert_eq!(store.marks.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn complete_prefetched_coverage_skips_dynamic_subscription_requery() {
        let provider = Arc::new(EmptyProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        store.coverage.lock().covered.push(request().range);
        let cached = CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        cached.prefetch_history(&[request()]).await.unwrap();
        cached.get_history(&request()).await.unwrap();

        assert_eq!(store.coverage_calls.load(Ordering::Relaxed), 1);
        assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn covered_subset_reuses_partial_universe_prefetch_coverage() {
        let provider = Arc::new(EmptyProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        let covered = TimeRange {
            start: NanosecondTimestamp(20),
            end: NanosecondTimestamp(40),
        };
        store.coverage.lock().covered.push(covered);
        let cached = CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        cached.prefetch_history(&[request()]).await.unwrap();
        cached
            .get_history(&request().with_range(covered))
            .await
            .unwrap();

        assert_eq!(store.coverage_calls.load(Ordering::Relaxed), 1);
        assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn provider_error_never_advances_coverage() {
        let provider = Arc::new(EmptyProvider {
            calls: AtomicUsize::new(0),
            fail: true,
        });
        let store = Arc::new(MemoryStore::default());
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();
        assert!(coordinator.get_history(&request()).await.is_err());
        assert!(coordinator.get_history(&request()).await.is_err());
        assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
        assert_eq!(store.marks.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn dropped_cache_append_still_returns_the_fetched_rows() {
        let provider = bar_provider();
        let store = Arc::new(MemoryStore {
            drop_appends: true,
            ..MemoryStore::default()
        });
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        let data = coordinator.get_history(&request()).await.unwrap();

        let HistoricalData::TradeBars(rows) = data else {
            unreachable!()
        };
        assert_eq!(rows, vec![provider.bar.clone()]);
        assert!(store.persisted.lock().is_empty());
    }

    #[tokio::test]
    async fn dropped_cache_append_leaves_the_range_uncovered() {
        let provider = bar_provider();
        let store = Arc::new(MemoryStore {
            drop_appends: true,
            ..MemoryStore::default()
        });
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        coordinator.get_history(&request()).await.unwrap();
        coordinator.get_history(&request()).await.unwrap();

        assert_eq!(store.marks.load(Ordering::Relaxed), 0);
        assert!(store.coverage.lock().covered.is_empty());
        assert_eq!(provider.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn persisted_cache_append_marks_coverage_and_serves_from_the_store() {
        let provider = bar_provider();
        let store = Arc::new(MemoryStore::default());
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        let first = coordinator.get_history(&request()).await.unwrap();
        let second = coordinator.get_history(&request()).await.unwrap();

        let HistoricalData::TradeBars(rows) = first else {
            unreachable!()
        };
        assert_eq!(rows, vec![provider.bar.clone()]);
        let HistoricalData::TradeBars(rows) = second else {
            unreachable!()
        };
        assert_eq!(rows, vec![provider.bar.clone()]);
        assert_eq!(store.marks.load(Ordering::Relaxed), 1);
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn partially_committed_dropped_batch_does_not_duplicate_rows() {
        let provider = bar_provider();
        let store = Arc::new(MemoryStore {
            drop_appends: true,
            ..MemoryStore::default()
        });
        // The cache reports an unknown commit state, so the rows may already
        // be durable. Merging the retained copy must not double them.
        store.persisted.lock().push(provider.bar.clone());
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        let data = coordinator.get_history(&request()).await.unwrap();

        let HistoricalData::TradeBars(rows) = data else {
            unreachable!()
        };
        assert_eq!(rows, vec![provider.bar.clone()]);
    }

    struct RatePublisher {
        rows: Vec<RiskFreeInterestRate>,
        requested: Mutex<Vec<TimeRange>>,
    }

    #[async_trait]
    impl HistoricalDataProvider for RatePublisher {
        fn name(&self) -> &str {
            "rates"
        }

        fn supports(&self, _request: &HistoryRequest) -> bool {
            false
        }

        async fn get_history(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
            bail!("the rate publisher serves no market data")
        }

        async fn get_risk_free_interest_rates(
            &self,
            range: TimeRange,
        ) -> Result<Option<Vec<RiskFreeInterestRate>>> {
            self.requested.lock().push(range);
            Ok(Some(
                self.rows
                    .iter()
                    .filter(|row| row.time >= range.start && row.time < range.end)
                    .cloned()
                    .collect(),
            ))
        }
    }

    fn rate(day: i64, annual_rate: Decimal) -> RiskFreeInterestRate {
        RiskFreeInterestRate::new(NanosecondTimestamp(day), annual_rate).with_venue("fred")
    }

    fn rate_range() -> TimeRange {
        TimeRange {
            start: NanosecondTimestamp(0),
            end: NanosecondTimestamp(100),
        }
    }

    #[tokio::test]
    async fn published_rates_are_persisted_and_returned() {
        let publisher = Arc::new(RatePublisher {
            rows: vec![
                rate(10, Decimal::new(225, 4)),
                rate(60, Decimal::new(55, 3)),
            ],
            requested: Mutex::new(Vec::new()),
        });
        let store = Arc::new(MemoryStore::default());
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![publisher.clone()]).unwrap();

        let rows = coordinator
            .get_risk_free_interest_rates(rate_range())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(rows, publisher.rows);
        assert_eq!(*store.rates.lock(), publisher.rows);
    }

    #[tokio::test]
    async fn cached_rates_refetch_prefix_before_earliest_and_suffix_after_newest() {
        let publisher = Arc::new(RatePublisher {
            rows: vec![rate(5, Decimal::new(15, 3)), rate(60, Decimal::new(55, 3))],
            requested: Mutex::new(Vec::new()),
        });
        let store = Arc::new(MemoryStore::default());
        store.rates.lock().push(rate(10, Decimal::new(225, 4)));
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![publisher.clone()]).unwrap();

        let rows = coordinator
            .get_risk_free_interest_rates(rate_range())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(
            *publisher.requested.lock(),
            vec![
                TimeRange {
                    start: NanosecondTimestamp(0),
                    end: NanosecondTimestamp(10),
                },
                TimeRange {
                    start: NanosecondTimestamp(11),
                    end: NanosecondTimestamp(100),
                },
            ]
        );
    }

    #[tokio::test]
    async fn cached_rates_spanning_the_request_do_not_refetch() {
        let publisher = Arc::new(RatePublisher {
            rows: vec![rate(60, Decimal::new(55, 3))],
            requested: Mutex::new(Vec::new()),
        });
        let store = Arc::new(MemoryStore::default());
        store
            .rates
            .lock()
            .extend([rate(0, Decimal::new(15, 3)), rate(99, Decimal::new(225, 4))]);
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![publisher.clone()]).unwrap();

        let rows = coordinator
            .get_risk_free_interest_rates(rate_range())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(rows.len(), 2);
        assert!(publisher.requested.lock().is_empty());
    }

    #[test]
    fn risk_free_missing_ranges_cover_prefix_and_suffix_only() {
        let requested = rate_range();
        assert_eq!(
            risk_free_rate_missing_ranges(requested, &[]),
            vec![requested]
        );
        assert_eq!(
            risk_free_rate_missing_ranges(requested, &[rate(40, Decimal::ONE)]),
            vec![
                TimeRange {
                    start: NanosecondTimestamp(0),
                    end: NanosecondTimestamp(40),
                },
                TimeRange {
                    start: NanosecondTimestamp(41),
                    end: NanosecondTimestamp(100),
                },
            ]
        );
        assert!(risk_free_rate_missing_ranges(
            requested,
            &[rate(0, Decimal::ONE), rate(99, Decimal::ONE)]
        )
        .is_empty());
    }

    #[tokio::test]
    async fn no_rate_publisher_and_no_cached_rows_reports_nothing_available() {
        let provider = Arc::new(EmptyProvider {
            calls: AtomicUsize::new(0),
            fail: false,
        });
        let store = Arc::new(MemoryStore::default());
        let coordinator = CacheFirstHistoryProvider::new(store, vec![provider]).unwrap();

        assert!(coordinator
            .get_risk_free_interest_rates(rate_range())
            .await
            .unwrap()
            .is_none());
    }

    #[test]
    fn merging_mismatched_history_families_is_an_error() {
        let mut bars = HistoricalData::TradeBars(Vec::new());
        assert!(bars.extend(HistoricalData::Ticks(Vec::new())).is_err());
    }

    #[test]
    fn dropped_cache_write_counter_totals_batches_and_rows() {
        let dropped = DroppedCacheWrites::default();
        assert_eq!(dropped.batches(), 0);

        dropped.record(120);
        dropped.record(80);

        assert_eq!(dropped.batches(), 2);
        assert_eq!(dropped.rows(), 200);
    }

    #[tokio::test]
    async fn custom_data_reads_the_operator_table_without_calling_market_provider() {
        let provider = Arc::new(EmptyProvider {
            calls: AtomicUsize::new(0),
            fail: true,
        });
        let store = Arc::new(MemoryStore::default());
        let coordinator =
            CacheFirstHistoryProvider::new(store.clone(), vec![provider.clone()]).unwrap();

        assert!(coordinator.supports(&custom_request()));
        coordinator.get_history(&custom_request()).await.unwrap();

        assert_eq!(provider.calls.load(Ordering::Relaxed), 0);
        assert_eq!(store.reads.load(Ordering::Relaxed), 1);
        assert_eq!(store.marks.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn canonical_option_sids_preserve_distinct_contracts_during_deduplication() {
        let date = NaiveDate::from_ymd_opt(2025, 1, 3).unwrap();
        let first_date = NaiveDate::from_ymd_opt(1998, 1, 2).unwrap();
        let option_row = |strike| OptionUniverseRow {
            date,
            market: "usa".to_string(),
            security_type: "Option".to_string(),
            symbol_sid: "SPY usa Option".to_string(),
            symbol_value: "SPY".to_string(),
            underlying_sid: Some("SPY usa Equity".to_string()),
            underlying_value: Some("SPY".to_string()),
            expiration: Some(date),
            strike: Some(strike),
            right: Some("Call".to_string()),
            open: Decimal::ZERO,
            high: Decimal::ZERO,
            low: Decimal::ZERO,
            close: Decimal::ZERO,
            volume: Decimal::ZERO,
            open_interest: None,
            implied_volatility: None,
            delta: None,
            gamma: None,
            vega: None,
            theta: None,
            rho: None,
        };
        let mut data = HistoricalData::OptionUniverse(vec![
            option_row(Decimal::new(500, 0)),
            option_row(Decimal::new(501, 0)),
        ]);
        let HistoricalData::OptionUniverse(rows) = &mut data else {
            unreachable!()
        };
        canonicalize_option_universe_rows(
            rows,
            "SPY",
            first_date,
            &Market::usa(),
            SecurityType::Equity,
        )
        .unwrap();
        data.sort_and_deduplicate();

        let HistoricalData::OptionUniverse(rows) = data else {
            unreachable!()
        };
        assert_eq!(rows.len(), 2);
        assert_ne!(rows[0].symbol_sid, rows[1].symbol_sid);
    }
}
