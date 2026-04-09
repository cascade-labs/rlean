/// Local disk-only history provider — reads Parquet trade bars with no network calls.
///
/// Useful as a fallback when data has already been downloaded to the local
/// Parquet store, or in tests.
use async_trait::async_trait;
use lean_data::TradeBar;
use lean_storage::{ParquetReader, PathResolver, QueryParams};

use crate::request::HistoryRequest;
use crate::traits::IHistoryProvider;

pub struct LocalHistoryProvider {
    pub(crate) data_root: std::path::PathBuf,
}

impl LocalHistoryProvider {
    pub fn new(data_root: impl AsRef<std::path::Path>) -> Self {
        LocalHistoryProvider {
            data_root: data_root.as_ref().to_path_buf(),
        }
    }
}

#[async_trait]
impl IHistoryProvider for LocalHistoryProvider {
    async fn get_history(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<TradeBar>> {
        let resolver = PathResolver::new(&self.data_root);

        // Build a glob that covers all date-partitioned files in the range.
        // For daily/weekly/monthly bars the path is not date-partitioned so
        // there is only one file; the predicate filters to the requested range.
        let start_date = request.start.date_utc();
        let end_date   = request.end.date_utc();

        // Collect candidate paths: iterate day-by-day for intraday resolutions,
        // or use the single non-partitioned path for daily/weekly/monthly.
        let paths: Vec<std::path::PathBuf> = if request.resolution.is_high_resolution() {
            let mut current = start_date;
            let mut v = Vec::new();
            while current <= end_date {
                let dp = resolver.trade_bar(&request.symbol, request.resolution, current);
                let p  = dp.to_path();
                if p.exists() {
                    v.push(p);
                }
                current = current.succ_opt().unwrap_or(current);
            }
            v
        } else {
            // Non-date-partitioned: single file keyed by the start date.
            let dp = resolver.trade_bar(&request.symbol, request.resolution, start_date);
            let p  = dp.to_path();
            if p.exists() { vec![p] } else { vec![] }
        };

        if paths.is_empty() {
            return Ok(vec![]);
        }

        let reader = ParquetReader::new();
        let params = QueryParams::new().with_time_range(request.start, request.end);

        let bars = reader
            .read_trade_bars(&paths, request.symbol.clone(), &params)
            .await
            .unwrap_or_default();

        Ok(bars)
    }
}
