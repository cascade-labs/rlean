use rlean_core::MarketHoursDatabase;
use rlean_data_sidecar::DataSidecarClient;
use rlean_data_tables::FactorFileEntry;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Bounds the adapter queue between each sidecar-backed enumerator and the
/// synchronizer. C# LEAN keeps one `Current` value per subscription and pulls
/// the next value on demand. Flight reads are batched, so a small bounded queue
/// preserves that pull/backpressure model without issuing one RPC per point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataFeedOptions {
    pub channel_capacity: usize,
}

impl Default for DataFeedOptions {
    fn default() -> Self {
        Self {
            channel_capacity: 16,
        }
    }
}

/// Shared subscription services. Market data comes exclusively from the
/// persistent Flight session; the engine owns only flow control and calendars.
#[derive(Clone)]
pub struct DataFeedContext {
    pub sidecar: Arc<DataSidecarClient>,
    pub options: DataFeedOptions,
    pub market_hours_database: Arc<MarketHoursDatabase>,
    unadjusted_equities: Arc<Mutex<HashSet<String>>>,
    auxiliary_factor_rows: Arc<Mutex<HashMap<String, Vec<FactorFileEntry>>>>,
}

impl DataFeedContext {
    pub fn new(sidecar: Arc<DataSidecarClient>) -> Self {
        Self {
            sidecar,
            options: DataFeedOptions::default(),
            market_hours_database: MarketHoursDatabase::global(),
            unadjusted_equities: Arc::new(Mutex::new(HashSet::new())),
            auxiliary_factor_rows: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn channel_capacity(&self) -> usize {
        self.options.channel_capacity.clamp(2, 1_024)
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
    fn subscription_queue_is_small_and_bounded_like_lean_current() {
        let options = DataFeedOptions::default();
        assert_eq!(options.channel_capacity, 16);
    }
}
