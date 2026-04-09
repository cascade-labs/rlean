use ::dashmap::DashMap;
use lean_data::TradeBar;
use std::sync::Arc;

type CacheKey = (u64, i64); // (symbol_sid, date as days-since-epoch)

/// In-memory LRU-ish data cache. Thread-safe via DashMap.
/// Keeps recently read bars in memory to avoid repeated parquet I/O.
pub struct DataCache {
    bars: DashMap<CacheKey, Arc<Vec<TradeBar>>>,
    max_entries: usize,
}

impl DataCache {
    pub fn new(max_entries: usize) -> Self {
        DataCache {
            bars: DashMap::new(),
            max_entries,
        }
    }

    pub fn get_bars(&self, symbol_sid: u64, date_days: i64) -> Option<Arc<Vec<TradeBar>>> {
        self.bars.get(&(symbol_sid, date_days)).map(|v| v.clone())
    }

    pub fn insert_bars(&self, symbol_sid: u64, date_days: i64, bars: Vec<TradeBar>) {
        // Simple eviction: if over capacity, clear everything.
        // A production implementation would use a proper LRU eviction policy.
        if self.bars.len() >= self.max_entries {
            self.bars.clear();
        }
        self.bars.insert((symbol_sid, date_days), Arc::new(bars));
    }

    pub fn invalidate(&self, symbol_sid: u64, date_days: i64) {
        self.bars.remove(&(symbol_sid, date_days));
    }

    pub fn clear(&self) {
        self.bars.clear();
    }

    pub fn len(&self) -> usize {
        self.bars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bars.is_empty()
    }
}

impl Default for DataCache {
    fn default() -> Self {
        DataCache::new(10_000)
    }
}
