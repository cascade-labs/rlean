use lean_core::MarketHoursDatabase;
use lean_data_providers::{ICustomDataSource, IHistoryProvider};
use lean_storage::IcebergStore;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionCachePolicy {
    /// Number of subscription intervals each stream may keep loaded ahead of the
    /// next algorithm-visible frontier.
    pub prefetch_intervals: usize,
    /// Safety cap for rows held in a per-subscription in-memory buffer.
    pub max_prefetch_rows: usize,
}

impl Default for SubscriptionCachePolicy {
    fn default() -> Self {
        Self {
            prefetch_intervals: 1,
            max_prefetch_rows: 100_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataFeedOptions {
    pub cache_policy: SubscriptionCachePolicy,
    pub fetch_missing_custom_data: bool,
    pub max_concurrent_market_fetches: usize,
    pub max_concurrent_market_appends: usize,
}

impl Default for DataFeedOptions {
    fn default() -> Self {
        Self {
            cache_policy: SubscriptionCachePolicy::default(),
            fetch_missing_custom_data: true,
            max_concurrent_market_fetches: 8,
            max_concurrent_market_appends: 2,
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
    pub market_append_permits: Arc<Semaphore>,
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
            market_append_permits: Arc::new(Semaphore::new(
                DataFeedOptions::default().max_concurrent_market_appends,
            )),
        }
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
}
