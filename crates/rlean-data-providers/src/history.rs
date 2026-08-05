use std::sync::Arc;

use anyhow::{bail, Result};
use async_trait::async_trait;
use rlean_core::{DateTime, Symbol};
use rlean_data::{SubscriptionDataConfig, SubscriptionDataKind};
use rlean_data_tables::{
    CustomDataPoint, FactorFileEntry, FundamentalUniverseRow, FutureUniverseRow, MapFileEntry,
    OptionUniverseRow, QuoteBar, Tick, TradeBar,
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

#[async_trait]
pub trait HistoricalDataProvider: Send + Sync {
    fn name(&self) -> &str;
    fn supports(&self, request: &HistoryRequest) -> bool;
    async fn get_history(&self, request: &HistoryRequest) -> Result<HistoricalData>;
    /// LEAN `IFactorFileProvider` boundary. `None` means unsupported; an empty
    /// vector is a successful provider response.
    async fn get_factor_file(&self, _symbol: &Symbol) -> Result<Option<Vec<FactorFileEntry>>> {
        Ok(None)
    }
    /// LEAN `IMapFileProvider` boundary.
    async fn get_map_file(&self, _symbol: &Symbol) -> Result<Option<Vec<MapFileEntry>>> {
        Ok(None)
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
    ) -> Result<()>;
    async fn mark_covered(&self, request: &HistoryRequest, provider: &str) -> Result<()>;
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
}

/// LEAN-style cache/provider composition: read persisted data first, request
/// only uncovered spans, persist successful results, then read one canonical
/// ordered result from the store. This also prevents provider overlap from
/// leaking duplicate bars into the subscription enumerator.
pub struct CacheFirstHistoryProvider {
    store: Arc<dyn HistoricalDataStore>,
    providers: Vec<Arc<dyn HistoricalDataProvider>>,
}

impl CacheFirstHistoryProvider {
    pub fn new(
        store: Arc<dyn HistoricalDataStore>,
        providers: Vec<Arc<dyn HistoricalDataProvider>>,
    ) -> Result<Self> {
        if providers.is_empty() {
            bail!("at least one historical data provider is required");
        }
        Ok(Self { store, providers })
    }
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
        let coverage = self.store.coverage(request).await?;
        for missing in coverage.missing(request.range) {
            let missing_request = request.with_range(missing);
            // Mark coverage only after a successful response. Empty success is
            // intentional negative knowledge and must survive restarts.
            let mut fetched = provider.get_history(&missing_request).await?;
            fetched.sort_and_deduplicate();
            if !fetched.is_empty() {
                self.store
                    .append(&missing_request, provider.name(), &fetched)
                    .await?;
            }
            self.store
                .mark_covered(&missing_request, provider.name())
                .await?;
        }
        let mut data = self.store.read(request).await?;
        data.sort_and_deduplicate();
        Ok(data)
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use rlean_core::{DataNormalizationMode, Market, NanosecondTimestamp, Resolution, Symbol};
    use rlean_data::{CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata};
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
        marks: AtomicUsize,
        reads: AtomicUsize,
    }

    #[async_trait]
    impl HistoricalDataStore for MemoryStore {
        async fn coverage(&self, _request: &HistoryRequest) -> Result<Coverage> {
            Ok(self.coverage.lock().clone())
        }

        async fn read(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(HistoricalData::TradeBars(Vec::new()))
        }

        async fn append(
            &self,
            _request: &HistoryRequest,
            _provider: &str,
            _data: &HistoricalData,
        ) -> Result<()> {
            Ok(())
        }

        async fn mark_covered(&self, request: &HistoryRequest, _provider: &str) -> Result<()> {
            self.marks.fetch_add(1, Ordering::Relaxed);
            self.coverage.lock().covered.push(request.range);
            Ok(())
        }
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
}
