use lean_core::{DateTime, Resolution, Result as LeanResult, Symbol};
use lean_data::{Slice, SubscriptionDataConfig};
use lean_storage::{DataCache, ParquetReader, PathResolver, QueryParams};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;

/// Loads market data for the engine's time loop.
pub struct DataManager {
    reader: Arc<ParquetReader>,
    resolver: PathResolver,
    cache: DataCache,
    subscriptions: Vec<SubscriptionDataConfig>,
}

impl DataManager {
    pub fn new(data_root: PathBuf) -> Self {
        DataManager {
            reader: Arc::new(ParquetReader::new()),
            resolver: PathResolver::new(data_root),
            cache: DataCache::new(50_000),
            subscriptions: Vec::new(),
        }
    }

    pub fn add_subscription(&mut self, config: SubscriptionDataConfig) {
        self.subscriptions.push(config);
    }

    /// Load all trade bars for a given date across all subscriptions.
    pub async fn get_slice_for_date(&self, date: chrono::NaiveDate) -> LeanResult<Slice> {
        use chrono::{TimeZone, Utc};
        let start = DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()));
        let end = DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59).unwrap()));

        let mut slice = Slice::new(start);

        for sub in &self.subscriptions {
            let sid = sub.symbol.id.sid;
            let day_key = date
                .signed_duration_since(chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
                .num_days();

            // Check cache first
            if let Some(cached) = self.cache.get_bars(sid, day_key) {
                for bar in cached.iter() {
                    slice.add_bar(bar.clone());
                }
                continue;
            }

            // Load from parquet
            let data_path = self.resolver.trade_bar(&sub.symbol, sub.resolution, date);
            let path = data_path.to_path();

            if path.exists() {
                let params = QueryParams::new().with_time_range(start, end);
                let bars = self
                    .reader
                    .read_trade_bars(&[path], sub.symbol.clone(), &params)
                    .await?;

                self.cache.insert_bars(sid, day_key, bars.clone());
                for bar in bars {
                    slice.add_bar(bar);
                }
            }
        }

        Ok(slice)
    }

    /// Preload bars for a date range into cache (parallel).
    pub async fn warm_cache(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> LeanResult<usize> {
        use chrono::Duration;
        let mut loaded = 0usize;
        let mut date = start;

        while date <= end {
            let data_path = self.resolver.trade_bar(symbol, resolution, date);
            let path = data_path.to_path();
            let sid = symbol.id.sid;
            let day_key = date
                .signed_duration_since(chrono::NaiveDate::from_ymd_opt(1, 1, 1).unwrap())
                .num_days();

            if path.exists() && self.cache.get_bars(sid, day_key).is_none() {
                use chrono::{TimeZone, Utc};
                let day_start =
                    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()));
                let day_end =
                    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59).unwrap()));

                let params = QueryParams::new().with_time_range(day_start, day_end);
                let bars = self
                    .reader
                    .read_trade_bars(&[path], symbol.clone(), &params)
                    .await?;
                loaded += bars.len();
                self.cache.insert_bars(sid, day_key, bars);
            }

            date += Duration::days(1);
        }

        debug!("Pre-cached {} bars for {}", loaded, symbol);
        Ok(loaded)
    }
}
