use rlean_core::MarketHoursDatabase;
use rlean_data_sidecar::DataSidecarClient;
use rlean_data_tables::FactorFileEntry;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

pub const CONSUMER_FRONTIER_UNSET: i64 = i64::MIN;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionBufferPolicy {
    pub prefetch_intervals: usize,
    pub max_prefetch_rows: usize,
    pub channel_capacity: usize,
    pub max_prefetch_ahead_days: i64,
}

impl SubscriptionBufferPolicy {
    pub fn for_active_subscriptions(count: usize) -> Self {
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

impl Default for SubscriptionBufferPolicy {
    fn default() -> Self {
        Self::for_active_subscriptions(1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DataFeedOptions {
    pub buffer_policy: SubscriptionBufferPolicy,
}

/// Shared subscription services. Market data comes exclusively from the
/// persistent Flight session; the engine owns only flow control and calendars.
#[derive(Clone)]
pub struct DataFeedContext {
    pub sidecar: Arc<DataSidecarClient>,
    pub options: DataFeedOptions,
    pub market_hours_database: Arc<MarketHoursDatabase>,
    pub active_subscription_count: Arc<AtomicUsize>,
    pub consumer_frontier_days: Arc<AtomicI64>,
    frontier_advanced: Arc<Notify>,
    unadjusted_equities: Arc<Mutex<HashSet<String>>>,
    auxiliary_factor_rows: Arc<Mutex<HashMap<String, Vec<FactorFileEntry>>>>,
}

impl DataFeedContext {
    pub fn new(sidecar: Arc<DataSidecarClient>) -> Self {
        Self {
            sidecar,
            options: DataFeedOptions::default(),
            market_hours_database: MarketHoursDatabase::global(),
            active_subscription_count: Arc::new(AtomicUsize::new(0)),
            consumer_frontier_days: Arc::new(AtomicI64::new(CONSUMER_FRONTIER_UNSET)),
            frontier_advanced: Arc::new(Notify::new()),
            unadjusted_equities: Arc::new(Mutex::new(HashSet::new())),
            auxiliary_factor_rows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn effective_buffer_policy(&self) -> SubscriptionBufferPolicy {
        let count = self.active_subscription_count.load(Ordering::Relaxed);
        SubscriptionBufferPolicy::for_active_subscriptions(count)
            .clamp_to(&self.options.buffer_policy)
    }

    pub fn observe_consumer_frontier(&self, date: chrono::NaiveDate) {
        let epoch = chrono::NaiveDate::from_ymd_opt(1970, 1, 1).expect("valid epoch");
        let days = date.signed_duration_since(epoch).num_days();
        let previous = self
            .consumer_frontier_days
            .fetch_max(days, Ordering::Relaxed);
        if days > previous {
            self.frontier_advanced.notify_waiters();
        }
    }

    pub fn seed_consumer_frontier(&self, date: chrono::NaiveDate) {
        self.observe_consumer_frontier(date);
    }

    pub fn frontier_advanced(&self) -> tokio::sync::futures::Notified<'_> {
        self.frontier_advanced.notified()
    }

    pub fn consumer_frontier_date(&self) -> Option<chrono::NaiveDate> {
        let days = self.consumer_frontier_days.load(Ordering::Relaxed);
        if days == CONSUMER_FRONTIER_UNSET {
            return None;
        }
        chrono::NaiveDate::from_ymd_opt(1970, 1, 1)?
            .checked_add_signed(chrono::Duration::days(days))
    }

    pub fn prefetch_ceiling_date(&self) -> Option<chrono::NaiveDate> {
        self.consumer_frontier_date()?
            .checked_add_signed(chrono::Duration::days(
                self.effective_buffer_policy().max_prefetch_ahead_days,
            ))
    }

    pub fn with_options(mut self, options: DataFeedOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_market_hours_database(
        mut self,
        market_hours_database: Arc<MarketHoursDatabase>,
    ) -> Self {
        self.market_hours_database = market_hours_database;
        self
    }

    pub fn record_unadjusted_equity(&self, ticker: &str) {
        if let Ok(mut set) = self.unadjusted_equities.lock() {
            set.insert(ticker.to_string());
        }
    }

    pub fn take_unadjusted_equities(&self) -> Vec<String> {
        let mut tickers: Vec<_> = self
            .unadjusted_equities
            .lock()
            .map(|mut set| set.drain().collect())
            .unwrap_or_default();
        tickers.sort();
        tickers
    }

    pub fn cached_auxiliary_factor_rows(&self, key: &str) -> Option<Vec<FactorFileEntry>> {
        self.auxiliary_factor_rows
            .lock()
            .ok()
            .and_then(|cache| cache.get(key).cloned())
    }

    pub fn cache_auxiliary_factor_rows(&self, key: String, rows: Vec<FactorFileEntry>) {
        if let Ok(mut cache) = self.auxiliary_factor_rows.lock() {
            cache.insert(key, rows);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_policy_bounds_large_universes() {
        let small = SubscriptionBufferPolicy::for_active_subscriptions(4);
        let large = SubscriptionBufferPolicy::for_active_subscriptions(512);
        assert!(small.channel_capacity > large.channel_capacity);
        assert!(small.max_prefetch_ahead_days > large.max_prefetch_ahead_days);
    }
}
