/// Local disk-only history provider — reads Parquet trade bars with no network calls.
///
/// Useful as a fallback when data has already been downloaded to the local
/// Parquet store, or in tests.
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

impl IHistoryProvider for LocalHistoryProvider {
    fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
        let resolver = PathResolver::new(&self.data_root);

        let start_date = request.start.date_utc();
        let end_date = request.end.date_utc();

        let paths: Vec<std::path::PathBuf> = if request.resolution.is_high_resolution() {
            let mut current = start_date;
            let mut v = Vec::new();
            while current <= end_date {
                let dp = resolver.trade_bar(&request.symbol, request.resolution, current);
                let p = dp.to_path();
                if p.exists() {
                    v.push(p);
                }
                current = current.succ_opt().unwrap_or(current);
            }
            v
        } else {
            let dp = resolver.trade_bar(&request.symbol, request.resolution, start_date);
            let p = dp.to_path();
            if p.exists() {
                vec![p]
            } else {
                vec![]
            }
        };

        if paths.is_empty() {
            return Ok(vec![]);
        }

        // ParquetReader::read_trade_bars is async; run it on a current-thread
        // runtime since get_history is called from spawn_blocking (no outer
        // runtime context is active on this thread).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!("failed to build local runtime: {e}"))?;

        let reader = ParquetReader::new();
        let params = QueryParams::new().with_time_range(request.start, request.end);
        let symbol = request.symbol.clone();

        let bars = rt
            .block_on(reader.read_trade_bars(&paths, symbol, &params))
            .unwrap_or_default();

        Ok(bars)
    }
}
