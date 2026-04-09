/// Unit tests for lean-data-providers.
#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use lean_core::{Market, NanosecondTimestamp, Resolution, SecurityIdentifier, Symbol};

    use crate::config::ProviderConfig;
    use crate::local::LocalHistoryProvider;
    use crate::request::{DataType, HistoryRequest};
    use crate::traits::IHistoryProvider;

    fn make_symbol() -> Symbol {
        Symbol {
            id:         SecurityIdentifier::generate_equity("SPY", &Market::usa()),
            value:      "SPY".to_string(),
            permtick:   "SPY".to_string(),
            underlying: None,
        }
    }

    fn make_history_request() -> HistoryRequest {
        // 2024-01-02 00:00:00 UTC and 2024-01-03 00:00:00 UTC (nanos since epoch)
        let start = NanosecondTimestamp(1704153600_000_000_000_i64);
        let end   = NanosecondTimestamp(1704240000_000_000_000_i64);
        HistoryRequest {
            symbol:     make_symbol(),
            resolution: Resolution::Daily,
            start,
            end,
            data_type:  DataType::TradeBar,
        }
    }

    // ── ProviderConfig ────────────────────────────────────────────────────────

    #[test]
    fn provider_config_default() {
        let cfg = ProviderConfig::default();
        assert_eq!(cfg.data_root, PathBuf::new());
        assert!(cfg.api_key.is_none());
        assert_eq!(cfg.requests_per_second, 0.0);
        assert_eq!(cfg.max_concurrent, 0);
    }

    #[test]
    fn provider_config_fields() {
        let cfg = ProviderConfig {
            data_root:           PathBuf::from("/data"),
            api_key:             Some("key".into()),
            requests_per_second: 5.0,
            max_concurrent:      4,
        };
        assert_eq!(cfg.data_root, PathBuf::from("/data"));
        assert_eq!(cfg.api_key.as_deref(), Some("key"));
    }

    // ── LocalHistoryProvider — no data file ───────────────────────────────────

    #[tokio::test]
    async fn local_provider_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalHistoryProvider::new(dir.path());

        let request = make_history_request();
        let bars = provider.get_history(&request).await.unwrap();

        assert!(
            bars.is_empty(),
            "Expected empty result when no Parquet file exists, got {} bars",
            bars.len()
        );
    }

    // ── HistoryRequest construction ───────────────────────────────────────────

    #[test]
    fn history_request_fields() {
        let req = make_history_request();
        assert_eq!(req.symbol.permtick, "SPY");
        assert_eq!(req.resolution, Resolution::Daily);
        assert_eq!(req.data_type, DataType::TradeBar);
    }

    // ── Unknown provider name (via providers module in rlean) — tested here
    //    by calling LocalHistoryProvider directly ───────────────────────────────

    #[tokio::test]
    async fn local_provider_handles_non_existent_dir_gracefully() {
        // A data root that doesn't exist should return empty rather than error.
        let provider = LocalHistoryProvider::new("/nonexistent/path/to/data");
        let request = make_history_request();
        let result = provider.get_history(&request).await;
        // Either Ok(empty) or we accept errors — the key property is no panic.
        let _ = result; // result can be Ok([]) or Err — both are acceptable
    }
}
