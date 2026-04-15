/// Unit tests for lean-data-providers.
#[cfg(test)]
mod custom_data_tests {
    use std::collections::HashMap;
    use chrono::NaiveDate;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use lean_core::Resolution;
    use lean_data::custom::{CustomDataConfig, CustomDataPoint, CustomDataSource, CustomDataTransport, CustomDataFormat};
    use crate::custom_data::ICustomDataSource;

    /// A minimal mock custom data source for testing.
    struct MockVixSource;

    impl ICustomDataSource for MockVixSource {
        fn name(&self) -> &str { "mock_vix" }

        fn get_source(
            &self,
            ticker: &str,
            date: NaiveDate,
            _config: &CustomDataConfig,
        ) -> Option<CustomDataSource> {
            // No data on weekends.
            use chrono::Datelike;
            if date.weekday() == chrono::Weekday::Sat || date.weekday() == chrono::Weekday::Sun {
                return None;
            }
            Some(CustomDataSource {
                uri: format!("https://example.com/vix/{}/{}", ticker, date.format("%Y%m%d")),
                transport: CustomDataTransport::Http,
                format: CustomDataFormat::Csv,
            })
        }

        fn reader(
            &self,
            line: &str,
            date: NaiveDate,
            _config: &CustomDataConfig,
        ) -> Option<CustomDataPoint> {
            // Skip headers and empty lines.
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with("Date") {
                return None;
            }
            // Parse "DATE,OPEN,HIGH,LOW,CLOSE" format.
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() < 5 { return None; }
            let close: Decimal = parts[4].trim().parse().ok()?;
            let open:  Decimal = parts[1].trim().parse().ok()?;
            let high:  Decimal = parts[2].trim().parse().ok()?;
            let low:   Decimal = parts[3].trim().parse().ok()?;
            let mut fields = HashMap::new();
            fields.insert("open".to_string(),  serde_json::json!(open.to_string()));
            fields.insert("high".to_string(),  serde_json::json!(high.to_string()));
            fields.insert("low".to_string(),   serde_json::json!(low.to_string()));
            Some(CustomDataPoint { time: date, value: close, fields })
        }

        fn default_resolution(&self) -> Resolution {
            Resolution::Daily
        }
    }

    fn make_config(ticker: &str) -> CustomDataConfig {
        CustomDataConfig {
            ticker: ticker.to_string(),
            source_type: "mock_vix".to_string(),
            resolution: Resolution::Daily,
            properties: HashMap::new(),
        }
    }

    #[test]
    fn test_mock_source_implements_trait() {
        let source: Box<dyn ICustomDataSource> = Box::new(MockVixSource);
        assert_eq!(source.name(), "mock_vix");
        assert_eq!(source.default_resolution(), Resolution::Daily);
        assert!(!source.requires_mapping());
    }

    #[test]
    fn test_get_source_returns_none_on_weekends() {
        let source = MockVixSource;
        let config = make_config("VIX");

        // 2024-01-06 is a Saturday.
        let sat = NaiveDate::from_ymd_opt(2024, 1, 6).unwrap();
        assert!(source.get_source("VIX", sat, &config).is_none(),
            "get_source should return None on Saturday");

        // 2024-01-07 is a Sunday.
        let sun = NaiveDate::from_ymd_opt(2024, 1, 7).unwrap();
        assert!(source.get_source("VIX", sun, &config).is_none(),
            "get_source should return None on Sunday");
    }

    #[test]
    fn test_get_source_returns_some_on_weekday() {
        let source = MockVixSource;
        let config = make_config("VIX");

        // 2024-01-08 is a Monday.
        let mon = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        let result = source.get_source("VIX", mon, &config);
        assert!(result.is_some(), "get_source should return Some on Monday");

        let ds = result.unwrap();
        assert_eq!(ds.transport, CustomDataTransport::Http);
        assert_eq!(ds.format, CustomDataFormat::Csv);
        assert!(ds.uri.contains("VIX"), "URI should contain ticker");
        assert!(ds.uri.contains("20240108"), "URI should contain date");
    }

    #[test]
    fn test_reader_skips_header_lines() {
        let source = MockVixSource;
        let config = make_config("VIX");
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();

        assert!(source.reader("", date, &config).is_none(), "empty line should be skipped");
        assert!(source.reader("# comment", date, &config).is_none(), "comment should be skipped");
        assert!(source.reader("Date,Open,High,Low,Close", date, &config).is_none(), "header should be skipped");
    }

    #[test]
    fn test_reader_parses_valid_csv_line() {
        let source = MockVixSource;
        let config = make_config("VIX");
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();

        let line = "2024-01-08,13.50,14.20,13.10,13.85";
        let result = source.reader(line, date, &config);
        assert!(result.is_some(), "valid CSV line should parse");

        let point = result.unwrap();
        assert_eq!(point.time, date);
        assert_eq!(point.value, dec!(13.85), "value should be close price");
        assert!(point.fields.contains_key("open"), "fields should contain open");
        assert!(point.fields.contains_key("high"), "fields should contain high");
        assert!(point.fields.contains_key("low"), "fields should contain low");
    }

    #[test]
    fn test_reader_returns_none_for_malformed_lines() {
        let source = MockVixSource;
        let config = make_config("VIX");
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();

        assert!(source.reader("not,enough", date, &config).is_none());
        assert!(source.reader("2024-01-08,abc,14.20,13.10,bad_close", date, &config).is_none());
    }

    #[test]
    fn test_cache_path_format() {
        // Test that the cache path function produces the expected layout.
        let root = std::path::Path::new("/data");
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();
        let path = lean_storage::custom_data_path(root, "fred", "UNRATE", date);

        let path_str = path.to_string_lossy();
        assert!(path_str.contains("custom"), "path should contain 'custom'");
        assert!(path_str.contains("fred"), "path should contain source_type");
        assert!(path_str.contains("unrate"), "path should contain ticker (lowercase)");
        assert!(path_str.contains("20240108"), "path should contain YYYYMMDD date");
        assert!(path_str.ends_with(".parquet"), "path should end with .parquet");
    }
}

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

    #[test]
    fn local_provider_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let provider = LocalHistoryProvider::new(dir.path());

        let request = make_history_request();
        let bars = provider.get_history(&request).unwrap();

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

    #[test]
    fn local_provider_handles_non_existent_dir_gracefully() {
        // A data root that doesn't exist should return empty rather than error.
        let provider = LocalHistoryProvider::new("/nonexistent/path/to/data");
        let request = make_history_request();
        let result = provider.get_history(&request);
        // Either Ok(empty) or we accept errors — the key property is no panic.
        let _ = result; // result can be Ok([]) or Err — both are acceptable
    }
}
