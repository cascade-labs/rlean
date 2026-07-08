use anyhow::Context;
use chrono::NaiveDate;
use lean_core::{MarketHoursDatabase, Resolution, SecurityType, Symbol, TickType};
use lean_data::{MarginInterestRate, PerpetualContext, QuoteBar, Tick, TradeBar};
use lean_data_providers::{HistoryRequest, ICustomDataSource, IHistoryProvider};
use lean_storage::{FactorFileEntry, IcebergStore, MapFileEntry, OptionEodBar};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};

/// Sentinel for "the consumer has not emitted any slice yet" in the shared
/// consumer-frontier atomic (stored as days since the Unix epoch).
pub const CONSUMER_FRONTIER_UNSET: i64 = i64::MIN;

/// Per-subscription in-memory prefetch limits.
///
/// C# LEAN backtests read subscription enumerators lazily from disk; rlean
/// producers stage a bounded window ahead of the synchronizer frontier and hand
/// points to the consumer over a bounded channel. Memory and provider load are
/// bounded by levers that scale with the active subscription count:
///
/// 1. `max_prefetch_ahead_days` — the balanced prefetch horizon. A producer may
///    only load/fetch partitions up to `consumer_frontier + max_prefetch_ahead_days`.
///    Small universes prefetch far ahead (streaming stays smooth); large
///    universes stay tight so hundreds of producers do not each race months into
///    the future fetching data the backtest clock has not reached (the cause of
///    the runaway RSS + wasteful re-fetch of not-yet-needed dates).
/// 2. `channel_capacity` — the producer blocks on a full channel (secondary
///    backpressure). The previous 100k-slot channel per stream was one source of
///    the multi-hundred-GB blowup on large universes.
///
/// `max_prefetch_rows` is only a pathological safety valve: it must never
/// truncate the legitimate rows of a single loaded partition (doing so silently
/// drops market/custom data and starves the algorithm), so it is kept large.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionCachePolicy {
    /// Number of subscription intervals each stream may load per partition read.
    pub prefetch_intervals: usize,
    /// Pathological safety cap on rows held in a producer's prefetch map. Sized
    /// far above any single partition so it never truncates real data.
    pub max_prefetch_rows: usize,
    /// Producer → consumer channel capacity. Secondary backpressure knob.
    pub channel_capacity: usize,
    /// How many calendar days ahead of the consumer frontier a producer may
    /// load/fetch. This is the primary balance between smooth streaming (large
    /// windows) and bounded memory/provider load on big universes (small windows).
    pub max_prefetch_ahead_days: i64,
}

impl SubscriptionCachePolicy {
    /// Derive prefetch limits from the current active subscription count.
    pub fn for_active_subscriptions(count: usize) -> Self {
        // Both knobs scale down as the universe grows. `channel_capacity * count`
        // and `ahead_days * count` stay roughly bounded, so a 1000-symbol
        // universe cannot collectively prefetch years of minute data at once.
        let (channel_capacity, max_prefetch_ahead_days) = match count.max(1) {
            1..=8 => (4_096, 365),
            9..=32 => (1_024, 90),
            33..=128 => (256, 21),
            129..=512 => (64, 7),
            _ => (64, 3),
        };
        Self {
            prefetch_intervals: 1,
            max_prefetch_rows: 100_000,
            channel_capacity,
            max_prefetch_ahead_days,
        }
    }

    /// Tokio channel capacity for producer → consumer handoff.
    pub fn channel_capacity(&self) -> usize {
        self.channel_capacity.clamp(16, 100_000)
    }

    pub fn clamp_to(self, ceiling: &Self) -> Self {
        Self {
            prefetch_intervals: self.prefetch_intervals.min(ceiling.prefetch_intervals),
            max_prefetch_rows: self.max_prefetch_rows.min(ceiling.max_prefetch_rows),
            channel_capacity: self.channel_capacity.min(ceiling.channel_capacity),
            max_prefetch_ahead_days: self
                .max_prefetch_ahead_days
                .min(ceiling.max_prefetch_ahead_days),
        }
    }
}

impl Default for SubscriptionCachePolicy {
    fn default() -> Self {
        Self::for_active_subscriptions(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderBackoffPolicy {
    pub max_attempts: usize,
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub recursive_split_min_days: i64,
}

impl Default for ProviderBackoffPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(5),
            recursive_split_min_days: 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataFeedOptions {
    pub cache_policy: SubscriptionCachePolicy,
    pub fetch_missing_custom_data: bool,
    pub max_concurrent_market_fetches: usize,
    pub max_concurrent_market_appends: usize,
    pub provider_backoff: ProviderBackoffPolicy,
}

impl Default for DataFeedOptions {
    fn default() -> Self {
        Self {
            cache_policy: SubscriptionCachePolicy::default(),
            fetch_missing_custom_data: true,
            max_concurrent_market_fetches: 8,
            max_concurrent_market_appends: 2,
            provider_backoff: ProviderBackoffPolicy::default(),
        }
    }
}

/// Shared data-feed services used by backtest and live subscription streams.
///
/// Runners should orchestrate algorithm lifecycle; provider/cache resolution
/// belongs in the feed layer so dynamically added subscriptions behave the same
/// as initial subscriptions.
#[derive(Clone)]
pub struct DataFeedContext {
    pub store: Arc<IcebergStore>,
    pub history_provider: Option<Arc<dyn IHistoryProvider>>,
    pub custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
    pub failed_custom_data_uris: Arc<Mutex<HashSet<String>>>,
    pub options: DataFeedOptions,
    pub market_hours_database: Arc<MarketHoursDatabase>,
    pub market_fetch_permits: Arc<Semaphore>,
    pub custom_fetch_permits: Arc<Semaphore>,
    pub market_append_permits: Arc<Semaphore>,
    /// Live count of active subscription streams; shared across cloned contexts.
    pub active_subscription_count: Arc<AtomicUsize>,
    /// Consumer frontier as days since the Unix epoch: the most recent slice date
    /// the synchronizer has emitted. Producers gate how far ahead they load/fetch
    /// against this so large universes cannot race months into the future.
    /// `CONSUMER_FRONTIER_UNSET` until the first slice is emitted.
    pub consumer_frontier_days: Arc<AtomicI64>,
    /// Wakes producer tasks that are blocked on the prefetch horizon when the
    /// synchronizer advances the consumer frontier.
    frontier_advanced: Arc<Notify>,
    market_cache_writes: Arc<MarketCacheWriteBuffer>,
    corporate_action_writes: Arc<CorporateActionWriteBuffer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MarketCacheKey {
    security_type: SecurityType,
    market: String,
    resolution: Resolution,
    tick_type: TickType,
}

#[derive(Default)]
struct MarketCacheWriteBuffer {
    trade_bars: Mutex<HashMap<MarketCacheKey, Vec<TradeBar>>>,
    quote_bars: Mutex<HashMap<MarketCacheKey, Vec<QuoteBar>>>,
    ticks: Mutex<HashMap<MarketCacheKey, Vec<Tick>>>,
}

#[derive(Default)]
struct CorporateActionWriteBuffer {
    factor_files: Mutex<Vec<(String, String, Vec<FactorFileEntry>)>>,
    map_files: Mutex<Vec<(String, String, Vec<MapFileEntry>)>>,
}

const MARKET_CACHE_FLUSH_CHUNK_ROWS: usize = 250_000;
const CORPORATE_ACTION_BUFFER_SYMBOLS: usize = 512;

impl MarketCacheWriteBuffer {
    fn key(
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> MarketCacheKey {
        MarketCacheKey {
            security_type,
            market: market.to_string(),
            resolution,
            tick_type,
        }
    }

    // Buffered rows are only ever flushed by consumer-frontier date (see
    // `drain_*_through`), never by row count: the frontier is the only signal
    // that tells us a row is actually safe to persist without racing ahead of
    // where the algorithm clock has gotten to. Do not add a size-triggered
    // flush here — that was dead code (a `usize::MAX` threshold that could
    // never fire) masking the fact that unbounded buffering is exactly what
    // the frontier-gated prefetch horizon in `prefetch_ceiling_date` exists to
    // prevent upstream, at fetch time.
    fn push_trade_bars(&self, bars: &[TradeBar], key: MarketCacheKey) {
        if bars.is_empty() {
            return;
        }
        self.trade_bars
            .lock()
            .expect("market cache buffer poisoned")
            .entry(key)
            .or_default()
            .extend_from_slice(bars);
    }

    fn push_quote_bars(&self, bars: &[QuoteBar], key: MarketCacheKey) {
        if bars.is_empty() {
            return;
        }
        self.quote_bars
            .lock()
            .expect("market cache buffer poisoned")
            .entry(key)
            .or_default()
            .extend_from_slice(bars);
    }

    fn push_ticks(&self, ticks: &[Tick], key: MarketCacheKey) {
        if ticks.is_empty() {
            return;
        }
        self.ticks
            .lock()
            .expect("market cache buffer poisoned")
            .entry(key)
            .or_default()
            .extend_from_slice(ticks);
    }

    fn drain_trade_bars(&self) -> Vec<(MarketCacheKey, Vec<TradeBar>)> {
        self.trade_bars
            .lock()
            .expect("market cache buffer poisoned")
            .drain()
            .filter(|(_, rows)| !rows.is_empty())
            .collect()
    }

    fn drain_trade_bars_through(&self, date: NaiveDate) -> Vec<(MarketCacheKey, Vec<TradeBar>)> {
        let mut buffers = self
            .trade_bars
            .lock()
            .expect("market cache buffer poisoned");
        drain_market_rows_through(&mut buffers, date, |row| row.end_time.date_utc())
    }

    fn drain_quote_bars(&self) -> Vec<(MarketCacheKey, Vec<QuoteBar>)> {
        self.quote_bars
            .lock()
            .expect("market cache buffer poisoned")
            .drain()
            .filter(|(_, rows)| !rows.is_empty())
            .collect()
    }

    fn drain_quote_bars_through(&self, date: NaiveDate) -> Vec<(MarketCacheKey, Vec<QuoteBar>)> {
        let mut buffers = self
            .quote_bars
            .lock()
            .expect("market cache buffer poisoned");
        drain_market_rows_through(&mut buffers, date, |row| row.end_time.date_utc())
    }

    fn drain_ticks(&self) -> Vec<(MarketCacheKey, Vec<Tick>)> {
        self.ticks
            .lock()
            .expect("market cache buffer poisoned")
            .drain()
            .filter(|(_, rows)| !rows.is_empty())
            .collect()
    }

    fn drain_ticks_through(&self, date: NaiveDate) -> Vec<(MarketCacheKey, Vec<Tick>)> {
        let mut buffers = self.ticks.lock().expect("market cache buffer poisoned");
        drain_market_rows_through(&mut buffers, date, |row| row.time.date_utc())
    }
}

fn drain_market_rows_through<T>(
    buffers: &mut HashMap<MarketCacheKey, Vec<T>>,
    date: NaiveDate,
    row_date: impl Fn(&T) -> NaiveDate,
) -> Vec<(MarketCacheKey, Vec<T>)> {
    let mut drained = Vec::new();
    buffers.retain(|key, rows| {
        let mut keep = Vec::with_capacity(rows.len());
        let mut flush = Vec::new();
        for row in rows.drain(..) {
            if row_date(&row) <= date {
                flush.push(row);
            } else {
                keep.push(row);
            }
        }
        *rows = keep;
        if !flush.is_empty() {
            drained.push((key.clone(), flush));
        }
        !rows.is_empty()
    });
    drained
}

impl CorporateActionWriteBuffer {
    fn push_factor_file(
        &self,
        market: String,
        ticker: String,
        rows: Vec<FactorFileEntry>,
    ) -> Option<Vec<(String, String, Vec<FactorFileEntry>)>> {
        if rows.is_empty() {
            return None;
        }
        let mut buffer = self
            .factor_files
            .lock()
            .expect("corporate action buffer poisoned");
        buffer.push((market, ticker, rows));
        (buffer.len() >= CORPORATE_ACTION_BUFFER_SYMBOLS).then(|| buffer.drain(..).collect())
    }

    fn push_map_file(
        &self,
        market: String,
        ticker: String,
        rows: Vec<MapFileEntry>,
    ) -> Option<Vec<(String, String, Vec<MapFileEntry>)>> {
        if rows.is_empty() {
            return None;
        }
        let mut buffer = self
            .map_files
            .lock()
            .expect("corporate action buffer poisoned");
        buffer.push((market, ticker, rows));
        (buffer.len() >= CORPORATE_ACTION_BUFFER_SYMBOLS).then(|| buffer.drain(..).collect())
    }

    fn drain_factor_files(&self) -> Vec<(String, String, Vec<FactorFileEntry>)> {
        self.factor_files
            .lock()
            .expect("corporate action buffer poisoned")
            .drain(..)
            .collect()
    }

    fn drain_map_files(&self) -> Vec<(String, String, Vec<MapFileEntry>)> {
        self.map_files
            .lock()
            .expect("corporate action buffer poisoned")
            .drain(..)
            .collect()
    }
}

fn provider_call_label(kind: &str, request: &HistoryRequest) -> String {
    format!(
        "{} {} {:?} {:?} {}..{}",
        kind,
        request.symbol.value,
        request.resolution,
        request.data_type,
        request.start.date_utc(),
        request.end.date_utc()
    )
}

impl DataFeedContext {
    pub fn new(store: Arc<IcebergStore>) -> Self {
        Self {
            store,
            history_provider: None,
            custom_data_sources: Vec::new(),
            failed_custom_data_uris: Arc::new(Mutex::new(HashSet::new())),
            options: DataFeedOptions::default(),
            market_hours_database: MarketHoursDatabase::global(),
            market_fetch_permits: Arc::new(Semaphore::new(
                DataFeedOptions::default().max_concurrent_market_fetches,
            )),
            custom_fetch_permits: Arc::new(Semaphore::new(
                DataFeedOptions::default().max_concurrent_market_fetches,
            )),
            market_append_permits: Arc::new(Semaphore::new(
                DataFeedOptions::default().max_concurrent_market_appends,
            )),
            active_subscription_count: Arc::new(AtomicUsize::new(0)),
            consumer_frontier_days: Arc::new(AtomicI64::new(CONSUMER_FRONTIER_UNSET)),
            frontier_advanced: Arc::new(Notify::new()),
            market_cache_writes: Arc::new(MarketCacheWriteBuffer::default()),
            corporate_action_writes: Arc::new(CorporateActionWriteBuffer::default()),
        }
    }

    pub fn effective_cache_policy(&self) -> SubscriptionCachePolicy {
        let count = self.active_subscription_count.load(Ordering::Relaxed);
        SubscriptionCachePolicy::for_active_subscriptions(count)
            .clamp_to(&self.options.cache_policy)
    }

    /// Record that the consumer (synchronizer) has emitted a slice at `date`.
    /// Monotonic: never moves the frontier backwards.
    pub fn observe_consumer_frontier(&self, date: chrono::NaiveDate) {
        let days = date
            .signed_duration_since(chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
            .num_days();
        let previous = self
            .consumer_frontier_days
            .fetch_max(days, Ordering::Relaxed);
        if days > previous {
            self.frontier_advanced.notify_waiters();
        }
    }

    /// Seed the consumer frontier before the first slice so producer tasks start
    /// with a finite prefetch horizon instead of treating startup as unbounded.
    pub fn seed_consumer_frontier(&self, date: chrono::NaiveDate) {
        self.observe_consumer_frontier(date);
    }

    /// Await a frontier advance using the standard Notify pattern: callers must
    /// create the waiter, re-check their condition, then await to avoid lost
    /// wakeups.
    pub fn frontier_advanced(&self) -> tokio::sync::futures::Notified<'_> {
        self.frontier_advanced.notified()
    }

    /// The consumer frontier as a date, or `None` before the first slice.
    pub fn consumer_frontier_date(&self) -> Option<chrono::NaiveDate> {
        let days = self.consumer_frontier_days.load(Ordering::Relaxed);
        if days == CONSUMER_FRONTIER_UNSET {
            return None;
        }
        chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
            .unwrap()
            .checked_add_signed(chrono::Duration::days(days))
    }

    /// Highest partition date a producer may load/fetch: the consumer frontier
    /// plus the policy's prefetch-ahead horizon. `None` (no ceiling) until the
    /// consumer has emitted its first slice, so warm-up/history can proceed.
    pub fn prefetch_ceiling_date(&self) -> Option<chrono::NaiveDate> {
        let frontier = self.consumer_frontier_date()?;
        let ahead = self.effective_cache_policy().max_prefetch_ahead_days;
        frontier.checked_add_signed(chrono::Duration::days(ahead))
    }

    pub fn with_history_provider(
        mut self,
        history_provider: Option<Arc<dyn IHistoryProvider>>,
    ) -> Self {
        self.history_provider = history_provider;
        self
    }

    pub fn with_custom_data_sources(
        mut self,
        custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
    ) -> Self {
        self.custom_data_sources = custom_data_sources;
        self
    }

    pub fn with_options(mut self, options: DataFeedOptions) -> Self {
        self.options = options;
        self.market_fetch_permits = Arc::new(Semaphore::new(options.max_concurrent_market_fetches));
        self.market_append_permits =
            Arc::new(Semaphore::new(options.max_concurrent_market_appends));
        self
    }

    pub fn with_market_hours_database(
        mut self,
        market_hours_database: Arc<MarketHoursDatabase>,
    ) -> Self {
        self.market_hours_database = market_hours_database;
        self
    }

    pub fn provider_backoff_policy(&self) -> ProviderBackoffPolicy {
        self.options.provider_backoff
    }

    pub fn is_retryable_provider_error(error: &anyhow::Error) -> bool {
        let message = error.to_string().to_ascii_lowercase();
        if message.starts_with("notimplemented:") {
            return false;
        }
        [
            "overload",
            "overloaded",
            "rate limit",
            "rate-limit",
            "rate_limit",
            "too many requests",
            "429",
            "timeout",
            "timed out",
            "temporar",
            "throttle",
            "throttled",
            "capacity",
            "connection reset",
            "connection refused",
            "connection closed",
            "unavailable",
            "busy",
        ]
        .iter()
        .any(|needle| message.contains(needle))
    }

    pub async fn execute_provider_call<T, F, Fut>(
        &self,
        label: impl Fn() -> String,
        mut call: F,
    ) -> anyhow::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = anyhow::Result<T>>,
    {
        let policy = self.provider_backoff_policy();
        let max_attempts = policy.max_attempts.max(1);
        let mut delay = policy.initial_delay;
        for attempt in 1..=max_attempts {
            let result = {
                let _fetch_permit = self
                    .market_fetch_permits
                    .clone()
                    .acquire_owned()
                    .await
                    .context("market fetch throttle closed")?;
                call().await
            };
            match result {
                Ok(value) => return Ok(value),
                Err(error)
                    if attempt < max_attempts && Self::is_retryable_provider_error(&error) =>
                {
                    tracing::debug!(
                        "Retrying provider call {} after retryable error (attempt {}/{}): {}",
                        label(),
                        attempt,
                        max_attempts,
                        error
                    );
                    tokio::time::sleep(delay).await;
                    delay = (delay.saturating_mul(2)).min(policy.max_delay);
                }
                Err(error) => return Err(error),
            }
        }
        unreachable!("provider retry loop always returns on final attempt")
    }

    pub async fn fetch_trade_bars(
        &self,
        provider: &dyn IHistoryProvider,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<TradeBar>> {
        self.execute_provider_call(
            || provider_call_label("trade bars", request),
            || provider.get_history(request),
        )
        .await
    }

    pub async fn fetch_quote_bars(
        &self,
        provider: &dyn IHistoryProvider,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<QuoteBar>> {
        self.execute_provider_call(
            || provider_call_label("quote bars", request),
            || provider.get_quote_bars(request),
        )
        .await
    }

    pub async fn fetch_ticks(
        &self,
        provider: &dyn IHistoryProvider,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<Tick>> {
        self.execute_provider_call(
            || provider_call_label("ticks", request),
            || provider.get_ticks(request),
        )
        .await
    }

    pub async fn fetch_perpetual_contexts(
        &self,
        provider: &dyn IHistoryProvider,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<PerpetualContext>> {
        self.execute_provider_call(
            || provider_call_label("perpetual contexts", request),
            || provider.get_perpetual_contexts(request),
        )
        .await
    }

    pub async fn fetch_margin_interest_rates(
        &self,
        provider: &dyn IHistoryProvider,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<MarginInterestRate>> {
        self.execute_provider_call(
            || provider_call_label("margin interest rates", request),
            || provider.get_margin_interest_rates(request),
        )
        .await
    }

    pub async fn fetch_factor_file(
        &self,
        provider: &dyn IHistoryProvider,
        symbol: &Symbol,
    ) -> anyhow::Result<Vec<FactorFileEntry>> {
        self.execute_provider_call(
            || format!("factor file {}", symbol.value),
            || provider.get_factor_file(symbol),
        )
        .await
    }

    pub async fn fetch_map_file(
        &self,
        provider: &dyn IHistoryProvider,
        symbol: &Symbol,
    ) -> anyhow::Result<Vec<MapFileEntry>> {
        self.execute_provider_call(
            || format!("map file {}", symbol.value),
            || provider.get_map_file(symbol),
        )
        .await
    }

    pub async fn fetch_option_eod_bars(
        &self,
        provider: &dyn IHistoryProvider,
        ticker: &str,
        date: NaiveDate,
    ) -> anyhow::Result<Vec<OptionEodBar>> {
        self.execute_provider_call(
            || format!("option eod bars {ticker} {date}"),
            || provider.get_option_eod_bars(ticker, date),
        )
        .await
    }

    pub async fn buffer_trade_bar_cache_write(
        &self,
        bars: &[TradeBar],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> anyhow::Result<()> {
        let key = MarketCacheWriteBuffer::key(security_type, market, resolution, tick_type);
        self.market_cache_writes.push_trade_bars(bars, key);
        Ok(())
    }

    pub async fn buffer_quote_bar_cache_write(
        &self,
        bars: &[QuoteBar],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> anyhow::Result<()> {
        let key = MarketCacheWriteBuffer::key(security_type, market, resolution, tick_type);
        self.market_cache_writes.push_quote_bars(bars, key);
        Ok(())
    }

    pub async fn buffer_tick_cache_write(
        &self,
        ticks: &[Tick],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> anyhow::Result<()> {
        let key = MarketCacheWriteBuffer::key(security_type, market, resolution, tick_type);
        self.market_cache_writes.push_ticks(ticks, key);
        Ok(())
    }

    /// Flush every buffered market row regardless of date. Used at backtest
    /// end (and after warm-up) to drain whatever the per-day flush in
    /// `flush_market_cache_writes_through` hasn't caught yet.
    pub async fn flush_market_cache_writes(&self) -> anyhow::Result<()> {
        self.flush_trade_bar_cache_writes(self.market_cache_writes.drain_trade_bars())
            .await?;
        self.flush_quote_bar_cache_writes(self.market_cache_writes.drain_quote_bars())
            .await?;
        self.flush_tick_cache_writes(self.market_cache_writes.drain_ticks())
            .await
    }

    /// Persist every buffered row whose bar/tick date has fully passed the
    /// synchronizer's frontier. Called once per algorithm day (see
    /// `DataManager::next_slice`) so buffered market rows never accumulate
    /// past a single day's worth of data in memory. Awaits the write directly
    /// (no background task): a failed flush must fail the backtest step that
    /// produced it, not surface later against an unrelated slice.
    pub async fn flush_market_cache_writes_through(&self, date: NaiveDate) -> anyhow::Result<()> {
        let trade_bars = self.market_cache_writes.drain_trade_bars_through(date);
        let quote_bars = self.market_cache_writes.drain_quote_bars_through(date);
        let ticks = self.market_cache_writes.drain_ticks_through(date);
        if trade_bars.is_empty() && quote_bars.is_empty() && ticks.is_empty() {
            return Ok(());
        }
        tokio::try_join!(
            self.flush_trade_bar_cache_writes(trade_bars),
            self.flush_quote_bar_cache_writes(quote_bars),
            self.flush_tick_cache_writes(ticks),
        )?;
        Ok(())
    }

    pub async fn buffer_factor_file_cache_write(
        &self,
        market: String,
        ticker: String,
        rows: Vec<FactorFileEntry>,
    ) -> anyhow::Result<()> {
        if let Some(flushes) = self
            .corporate_action_writes
            .push_factor_file(market, ticker, rows)
        {
            self.flush_factor_file_cache_writes(flushes).await?;
        }
        Ok(())
    }

    pub async fn buffer_map_file_cache_write(
        &self,
        market: String,
        ticker: String,
        rows: Vec<MapFileEntry>,
    ) -> anyhow::Result<()> {
        if let Some(flushes) = self
            .corporate_action_writes
            .push_map_file(market, ticker, rows)
        {
            self.flush_map_file_cache_writes(flushes).await?;
        }
        Ok(())
    }

    pub async fn flush_corporate_action_cache_writes(&self) -> anyhow::Result<()> {
        self.flush_factor_file_cache_writes(self.corporate_action_writes.drain_factor_files())
            .await?;
        self.flush_map_file_cache_writes(self.corporate_action_writes.drain_map_files())
            .await
    }

    async fn flush_factor_file_cache_writes(
        &self,
        rows: Vec<(String, String, Vec<FactorFileEntry>)>,
    ) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.store.append_factor_files(&rows).await
    }

    async fn flush_map_file_cache_writes(
        &self,
        rows: Vec<(String, String, Vec<MapFileEntry>)>,
    ) -> anyhow::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.store.append_map_files(&rows).await
    }

    async fn flush_trade_bar_cache_writes(
        &self,
        flushes: Vec<(MarketCacheKey, Vec<TradeBar>)>,
    ) -> anyhow::Result<()> {
        for (key, rows) in flushes {
            let _append_permit = self
                .market_append_permits
                .clone()
                .acquire_owned()
                .await
                .context("market append throttle closed")?;
            for chunk in rows.chunks(MARKET_CACHE_FLUSH_CHUNK_ROWS) {
                self.store
                    .append_trade_bars_unchecked(
                        chunk,
                        key.security_type,
                        &key.market,
                        key.resolution,
                        key.tick_type,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn flush_quote_bar_cache_writes(
        &self,
        flushes: Vec<(MarketCacheKey, Vec<QuoteBar>)>,
    ) -> anyhow::Result<()> {
        for (key, rows) in flushes {
            let _append_permit = self
                .market_append_permits
                .clone()
                .acquire_owned()
                .await
                .context("market append throttle closed")?;
            for chunk in rows.chunks(MARKET_CACHE_FLUSH_CHUNK_ROWS) {
                self.store
                    .append_quote_bars_unchecked(
                        chunk,
                        key.security_type,
                        &key.market,
                        key.resolution,
                        key.tick_type,
                    )
                    .await?;
            }
        }
        Ok(())
    }

    async fn flush_tick_cache_writes(
        &self,
        flushes: Vec<(MarketCacheKey, Vec<Tick>)>,
    ) -> anyhow::Result<()> {
        for (key, rows) in flushes {
            let _append_permit = self
                .market_append_permits
                .clone()
                .acquire_owned()
                .await
                .context("market append throttle closed")?;
            for chunk in rows.chunks(MARKET_CACHE_FLUSH_CHUNK_ROWS) {
                self.store
                    .append_ticks(
                        chunk,
                        key.security_type,
                        &key.market,
                        key.resolution,
                        key.tick_type,
                    )
                    .await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lean_core::{DateTime, Market, Symbol, TimeSpan};
    use lean_data::TradeBarData;
    use rust_decimal_macros::dec;

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(chrono::Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    fn trade_bar(symbol: &Symbol, date: NaiveDate) -> TradeBar {
        TradeBar::new(
            symbol.clone(),
            dt(date, 9, 30),
            TimeSpan::ONE_HOUR,
            TradeBarData::new(dec!(1), dec!(2), dec!(1), dec!(2), dec!(100)),
        )
    }

    #[test]
    fn adaptive_policy_scales_channel_down_for_large_universes() {
        let small = SubscriptionCachePolicy::for_active_subscriptions(4);
        let medium = SubscriptionCachePolicy::for_active_subscriptions(64);
        let large = SubscriptionCachePolicy::for_active_subscriptions(512);

        assert!(small.channel_capacity > medium.channel_capacity);
        assert!(medium.channel_capacity > large.channel_capacity);
        // The safety valve stays large so it never truncates a real partition.
        assert_eq!(small.max_prefetch_rows, large.max_prefetch_rows);
        assert!(large.max_prefetch_rows >= 100_000);
    }

    #[test]
    fn adaptive_policy_scales_prefetch_horizon_down_for_large_universes() {
        let small = SubscriptionCachePolicy::for_active_subscriptions(4);
        let medium = SubscriptionCachePolicy::for_active_subscriptions(64);
        let large = SubscriptionCachePolicy::for_active_subscriptions(512);
        let huge = SubscriptionCachePolicy::for_active_subscriptions(2_000);

        assert!(small.max_prefetch_ahead_days > medium.max_prefetch_ahead_days);
        assert!(medium.max_prefetch_ahead_days > large.max_prefetch_ahead_days);
        assert!(large.max_prefetch_ahead_days >= huge.max_prefetch_ahead_days);
        // Even the smallest horizon still lets a stream stay a few days ahead.
        assert!(huge.max_prefetch_ahead_days >= 1);
    }

    #[test]
    fn configured_ceiling_caps_adaptive_policy() {
        let adaptive = SubscriptionCachePolicy::for_active_subscriptions(4);
        let ceiling = SubscriptionCachePolicy {
            prefetch_intervals: 2,
            max_prefetch_rows: 100_000,
            channel_capacity: 48,
            max_prefetch_ahead_days: 5,
        };
        let policy = adaptive.clamp_to(&ceiling);
        assert_eq!(policy.channel_capacity, 48);
        assert_eq!(policy.max_prefetch_ahead_days, 5);
    }

    #[test]
    fn prefetch_ceiling_tracks_consumer_frontier() {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(IcebergStore::connect_local(tmp.path()))
                .unwrap(),
        );
        let ctx = DataFeedContext::new(store);
        // No frontier yet → no ceiling (warm-up proceeds unthrottled).
        assert!(ctx.prefetch_ceiling_date().is_none());

        let day = chrono::NaiveDate::from_ymd_opt(2022, 1, 5).unwrap();
        ctx.observe_consumer_frontier(day);
        let ceiling = ctx.prefetch_ceiling_date().expect("ceiling after frontier");
        // Single subscription → 365-day horizon.
        assert_eq!(ceiling, day + chrono::Duration::days(365));

        // Frontier is monotonic: an older date does not move it back.
        ctx.observe_consumer_frontier(chrono::NaiveDate::from_ymd_opt(2022, 1, 1).unwrap());
        assert_eq!(ctx.consumer_frontier_date(), Some(day));
    }

    #[test]
    fn frontier_notifier_wakes_on_advancing_frontier_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(IcebergStore::connect_local(tmp.path()))
                .unwrap(),
        );
        let ctx = DataFeedContext::new(store);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let day = chrono::NaiveDate::from_ymd_opt(2022, 1, 5).unwrap();
            let notified = ctx.frontier_advanced();
            ctx.observe_consumer_frontier(day);
            notified.await;

            let stale = ctx.frontier_advanced();
            ctx.observe_consumer_frontier(chrono::NaiveDate::from_ymd_opt(2022, 1, 4).unwrap());
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), stale)
                    .await
                    .is_err()
            );
        });
    }

    #[test]
    fn market_cache_drain_through_flushes_only_mature_trade_rows() {
        let buffer = MarketCacheWriteBuffer::default();
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2022, 1, 3).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2022, 1, 4).unwrap();
        let day3 = NaiveDate::from_ymd_opt(2022, 1, 5).unwrap();
        let key = MarketCacheWriteBuffer::key(
            SecurityType::Equity,
            Market::usa().as_str(),
            Resolution::Daily,
            TickType::Trade,
        );

        buffer.push_trade_bars(
            &[
                trade_bar(&symbol, day1),
                trade_bar(&symbol, day2),
                trade_bar(&symbol, day3),
            ],
            key.clone(),
        );

        let first_flush = buffer.drain_trade_bars_through(day2);
        assert_eq!(first_flush.len(), 1);
        assert_eq!(first_flush[0].0, key);
        assert_eq!(first_flush[0].1.len(), 2);
        assert!(first_flush[0]
            .1
            .iter()
            .all(|bar| bar.end_time.date_utc() <= day2));

        let second_flush = buffer.drain_trade_bars_through(day2);
        assert!(second_flush.is_empty());

        let remaining = buffer.drain_trade_bars();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].1.len(), 1);
        assert_eq!(remaining[0].1[0].end_time.date_utc(), day3);
    }

    #[test]
    fn market_cache_drain_through_preserves_grouped_keys() {
        let buffer = MarketCacheWriteBuffer::default();
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2022, 1, 3).unwrap();
        let daily_key = MarketCacheWriteBuffer::key(
            SecurityType::Equity,
            Market::usa().as_str(),
            Resolution::Daily,
            TickType::Trade,
        );
        let minute_key = MarketCacheWriteBuffer::key(
            SecurityType::Equity,
            Market::usa().as_str(),
            Resolution::Minute,
            TickType::Trade,
        );

        buffer.push_trade_bars(&[trade_bar(&spy, day)], daily_key.clone());
        buffer.push_trade_bars(&[trade_bar(&spy, day)], minute_key.clone());

        let flush = buffer.drain_trade_bars_through(day);
        let keys: HashSet<_> = flush.into_iter().map(|(key, _)| key).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&daily_key));
        assert!(keys.contains(&minute_key));
    }
}
