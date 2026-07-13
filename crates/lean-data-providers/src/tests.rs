/// Unit tests for lean-data-providers.
#[cfg(test)]
mod custom_data_tests {
    use crate::custom_data::ICustomDataSource;
    use chrono::NaiveDate;
    use lean_core::Resolution;
    use lean_data::custom::{
        CustomDataConfig, CustomDataFormat, CustomDataPoint, CustomDataSource, CustomDataTransport,
    };
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::sync::Arc;

    /// A minimal mock custom data source for testing.
    struct MockVixSource;

    impl ICustomDataSource for MockVixSource {
        fn name(&self) -> &str {
            "mock_vix"
        }

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
                uri: format!(
                    "https://example.com/vix/{}/{}",
                    ticker,
                    date.format("%Y%m%d")
                ),
                transport: CustomDataTransport::Http,
                format: CustomDataFormat::Csv,
                headers: HashMap::new(),
                symbol_column: None,
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
            if parts.len() < 5 {
                return None;
            }
            let close: Decimal = parts[4].trim().parse().ok()?;
            let open: Decimal = parts[1].trim().parse().ok()?;
            let high: Decimal = parts[2].trim().parse().ok()?;
            let low: Decimal = parts[3].trim().parse().ok()?;
            let mut fields = HashMap::new();
            fields.insert("open".to_string(), serde_json::json!(open.to_string()));
            fields.insert("high".to_string(), serde_json::json!(high.to_string()));
            fields.insert("low".to_string(), serde_json::json!(low.to_string()));
            Some(CustomDataPoint {
                time: date,
                end_time: None,
                value: close,
                symbol: None,
                fields: Arc::new(fields),
            })
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
            query: Default::default(),
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
        assert!(
            source.get_source("VIX", sat, &config).is_none(),
            "get_source should return None on Saturday"
        );

        // 2024-01-07 is a Sunday.
        let sun = NaiveDate::from_ymd_opt(2024, 1, 7).unwrap();
        assert!(
            source.get_source("VIX", sun, &config).is_none(),
            "get_source should return None on Sunday"
        );
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

        assert!(
            source.reader("", date, &config).is_none(),
            "empty line should be skipped"
        );
        assert!(
            source.reader("# comment", date, &config).is_none(),
            "comment should be skipped"
        );
        assert!(
            source
                .reader("Date,Open,High,Low,Close", date, &config)
                .is_none(),
            "header should be skipped"
        );
    }

    #[test]
    fn test_reader_parses_valid_data_line() {
        let source = MockVixSource;
        let config = make_config("VIX");
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();

        let line = "2024-01-08,13.50,14.20,13.10,13.85";
        let result = source.reader(line, date, &config);
        assert!(result.is_some(), "valid data line should parse");

        let point = result.unwrap();
        assert_eq!(point.time, date);
        assert_eq!(point.value, dec!(13.85), "value should be close price");
        assert!(
            point.fields.contains_key("open"),
            "fields should contain open"
        );
        assert!(
            point.fields.contains_key("high"),
            "fields should contain high"
        );
        assert!(
            point.fields.contains_key("low"),
            "fields should contain low"
        );
    }

    #[test]
    fn test_reader_returns_none_for_malformed_lines() {
        let source = MockVixSource;
        let config = make_config("VIX");
        let date = NaiveDate::from_ymd_opt(2024, 1, 8).unwrap();

        assert!(source.reader("not,enough", date, &config).is_none());
        assert!(source
            .reader("2024-01-08,abc,14.20,13.10,bad_close", date, &config)
            .is_none());
    }
}

#[cfg(test)]
mod provider_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{
        Market, NanosecondTimestamp, Resolution, SecurityIdentifier, Symbol, TickType, TimeSpan,
    };
    use lean_data::{TradeBar, TradeBarData};
    use lean_storage::{
        IcebergStore, OptionEodBar, OptionUniverseRow, RestCatalogConfig, SigV4Config,
    };
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    use crate::config::ProviderConfig;
    use crate::local::LocalHistoryProvider;
    use crate::request::{DataType, HistoryBatchRequest, HistoryRequest, MarketDataBatch};
    use crate::stacked::StackedHistoryProvider;
    use crate::traits::IHistoryProvider;

    fn make_symbol_for(ticker: &str) -> Symbol {
        Symbol::from_parts(
            SecurityIdentifier::generate_equity(ticker, &Market::usa()),
            ticker.to_string(),
            ticker.to_string(),
            None,
        )
    }

    fn make_symbol() -> Symbol {
        make_symbol_for("SPY")
    }

    fn make_history_request() -> HistoryRequest {
        // 2024-01-02 00:00:00 UTC and 2024-01-03 00:00:00 UTC (nanos since epoch)
        let start = NanosecondTimestamp(1_704_153_600_000_000_000_i64);
        let end = NanosecondTimestamp(1_704_240_000_000_000_000_i64);
        HistoryRequest {
            symbol: make_symbol(),
            resolution: Resolution::Daily,
            start,
            end,
            data_type: DataType::TradeBar,
        }
    }

    fn make_history_request_for_range(start: NaiveDate, end: NaiveDate) -> HistoryRequest {
        HistoryRequest {
            symbol: make_symbol(),
            resolution: Resolution::Daily,
            start: date_time(start, 0, 0, 0),
            end: date_time(end, 23, 59, 59),
            data_type: DataType::TradeBar,
        }
    }

    fn make_history_batch_request(symbols: Vec<Symbol>) -> HistoryBatchRequest {
        HistoryBatchRequest {
            symbols,
            resolution: Resolution::Daily,
            start: date_time(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(), 0, 0, 0),
            end: date_time(NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(), 0, 0, 0),
            data_type: DataType::TradeBar,
        }
    }

    fn date_time(date: NaiveDate, h: u32, m: u32, s: u32) -> NanosecondTimestamp {
        NanosecondTimestamp::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap()))
    }

    fn make_bar(date: NaiveDate) -> TradeBar {
        make_bar_for(make_symbol(), date)
    }

    fn make_bar_for(symbol: Symbol, date: NaiveDate) -> TradeBar {
        TradeBar::new(
            symbol,
            date_time(date - chrono::Duration::days(1), 16, 0, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
        )
    }

    fn make_start_dated_bar_for(symbol: Symbol, date: NaiveDate) -> TradeBar {
        TradeBar::new(
            symbol,
            date_time(date, 16, 0, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
        )
    }

    /// Connect to the test REST Iceberg catalog, or return `None` when it is not
    /// configured (so the caller skips). There is no local/filesystem catalog
    /// anymore; these tests need a live REST catalog.
    ///
    /// Reads `RLEAN_TEST_CATALOG` (base URI; unset => skip), `RLEAN_TEST_WAREHOUSE`,
    /// optional `RLEAN_TEST_SIGV4_REGION` (+ `RLEAN_TEST_SIGV4_NAME`, default
    /// `s3tables`) and `RLEAN_TEST_NAMESPACE` (default `lean_dev`, an isolated
    /// scratch namespace).
    async fn connect_test_store() -> Option<Arc<IcebergStore>> {
        let uri = std::env::var("RLEAN_TEST_CATALOG")
            .ok()
            .filter(|v| !v.is_empty())?;
        let warehouse = std::env::var("RLEAN_TEST_WAREHOUSE")
            .expect("RLEAN_TEST_WAREHOUSE must be set when RLEAN_TEST_CATALOG is");
        let sigv4 = std::env::var("RLEAN_TEST_SIGV4_REGION")
            .ok()
            .filter(|region| !region.is_empty())
            .map(|region| SigV4Config {
                region,
                signing_name: std::env::var("RLEAN_TEST_SIGV4_NAME")
                    .ok()
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "s3tables".to_string()),
            });
        let namespace = std::env::var("RLEAN_TEST_NAMESPACE")
            .ok()
            .filter(|ns| !ns.is_empty())
            .unwrap_or_else(|| "lean_dev".to_string());
        Some(Arc::new(
            IcebergStore::connect(RestCatalogConfig {
                uri,
                warehouse,
                sigv4,
                namespace,
                data_refresh_secs: 0,
            })
            .await
            .expect("failed to connect to the test REST catalog"),
        ))
    }

    /// Build a `LocalHistoryProvider` over the test catalog store, or `None`
    /// when the catalog is not configured.
    async fn local_provider_with_store() -> Option<(LocalHistoryProvider, Arc<IcebergStore>)> {
        let store = connect_test_store().await?;
        Some((LocalHistoryProvider::from_store(store.clone()), store))
    }

    fn make_option_universe_row(
        date: NaiveDate,
        underlying: &str,
        symbol_value: &str,
    ) -> OptionUniverseRow {
        OptionUniverseRow {
            date,
            symbol_value: symbol_value.to_string(),
            underlying: underlying.to_string(),
            expiration: NaiveDate::from_ymd_opt(2024, 2, 16).unwrap(),
            strike: dec!(100),
            right: "C".to_string(),
        }
    }

    async fn write_daily_bars(store: &IcebergStore, bars: &[TradeBar]) {
        store
            .append_trade_bars(
                bars,
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
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

    #[derive(Clone)]
    struct MockOptionProvider {
        option_rows: Vec<OptionEodBar>,
        earliest: Option<NaiveDate>,
    }

    #[async_trait::async_trait]
    impl IHistoryProvider for MockOptionProvider {
        async fn get_history(
            &self,
            _request: &HistoryRequest,
        ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
            Ok(vec![])
        }

        async fn get_option_eod_bars(
            &self,
            _ticker: &str,
            _date: NaiveDate,
        ) -> anyhow::Result<Vec<OptionEodBar>> {
            Ok(self.option_rows.clone())
        }

        fn earliest_date(&self) -> Option<NaiveDate> {
            self.earliest
        }
    }

    #[derive(Clone)]
    struct MockFactorRowsProvider {
        implemented: bool,
    }

    #[async_trait::async_trait]
    impl IHistoryProvider for MockFactorRowsProvider {
        async fn get_history(
            &self,
            request: &HistoryRequest,
        ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
            if !self.implemented {
                anyhow::bail!("NotImplemented: no {:?}", request.data_type);
            }
            Ok(vec![])
        }
    }

    #[derive(Clone)]
    struct RecordingEmptyRowsProvider {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl IHistoryProvider for RecordingEmptyRowsProvider {
        async fn get_history(
            &self,
            _request: &HistoryRequest,
        ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![])
        }
    }

    #[derive(Clone)]
    struct RecordingOptionUniverseProvider {
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl IHistoryProvider for RecordingOptionUniverseProvider {
        async fn get_history(
            &self,
            _request: &HistoryRequest,
        ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
            Ok(vec![])
        }

        async fn get_option_universe(
            &self,
            ticker: &str,
            date: NaiveDate,
        ) -> anyhow::Result<Vec<OptionUniverseRow>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(vec![OptionUniverseRow {
                date,
                symbol_value: format!("{ticker}260417C00050000"),
                underlying: ticker.to_string(),
                expiration: NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
                strike: dec!(50),
                right: "C".to_string(),
            }])
        }
    }

    #[derive(Clone)]
    struct FailingSymbolProvider {
        failed_symbol: String,
    }

    #[async_trait::async_trait]
    impl IHistoryProvider for FailingSymbolProvider {
        async fn get_history(
            &self,
            request: &HistoryRequest,
        ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
            if request.symbol.value.as_ref() == self.failed_symbol {
                anyhow::bail!("provider has no data for {}", request.symbol.value);
            }
            Ok(vec![make_bar_for(
                request.symbol.clone(),
                request.start.date_utc(),
            )])
        }
    }

    #[derive(Clone)]
    struct MockBatchProvider {
        fail_batch: bool,
        bars: Vec<TradeBar>,
    }

    #[async_trait::async_trait]
    impl IHistoryProvider for MockBatchProvider {
        async fn get_history(
            &self,
            _request: &HistoryRequest,
        ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
            Ok(vec![])
        }

        async fn get_history_batch(
            &self,
            _request: &HistoryBatchRequest,
        ) -> anyhow::Result<MarketDataBatch> {
            if self.fail_batch {
                anyhow::bail!("provider batch request failed");
            }
            Ok(MarketDataBatch {
                trade_bars: self.bars.clone(),
                ..Default::default()
            })
        }
    }

    fn sample_option_row() -> OptionEodBar {
        OptionEodBar {
            date: NaiveDate::from_ymd_opt(2026, 4, 17).unwrap(),
            symbol_value: "TLT260515P00100000".to_string(),
            underlying: "TLT".to_string(),
            expiration: NaiveDate::from_ymd_opt(2026, 5, 15).unwrap(),
            strike: Decimal::new(1000, 1),
            right: "P".to_string(),
            open: Decimal::new(100, 2),
            high: Decimal::new(125, 2),
            low: Decimal::new(95, 2),
            close: Decimal::new(110, 2),
            volume: 10,
            bid: Decimal::new(105, 2),
            ask: Decimal::new(115, 2),
            bid_size: 3,
            ask_size: 4,
        }
    }

    #[tokio::test]
    async fn stacked_provider_forwards_option_eod_requests() {
        let provider = StackedHistoryProvider::new(vec![Arc::new(MockOptionProvider {
            option_rows: vec![sample_option_row()],
            earliest: Some(NaiveDate::from_ymd_opt(2018, 1, 1).unwrap()),
        })]);

        let rows = provider
            .get_option_eod_bars("TLT", NaiveDate::from_ymd_opt(2026, 4, 17).unwrap())
            .await
            .unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].underlying, "TLT");
    }

    #[tokio::test]
    async fn stacked_provider_falls_back_for_factor_rows_not_implemented() {
        let provider = StackedHistoryProvider::new(vec![
            Arc::new(MockFactorRowsProvider { implemented: false }),
            Arc::new(MockFactorRowsProvider { implemented: true }),
        ]);
        let mut request = make_history_request();
        request.data_type = DataType::FactorFile;

        let rows = provider.get_history(&request).await.unwrap();

        assert!(rows.is_empty());
    }

    #[tokio::test]
    async fn stacked_provider_tries_next_factor_provider_after_empty_ok() {
        let first_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let second_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = StackedHistoryProvider::new(vec![
            Arc::new(RecordingEmptyRowsProvider {
                calls: Arc::clone(&first_calls),
            }),
            Arc::new(RecordingEmptyRowsProvider {
                calls: Arc::clone(&second_calls),
            }),
        ]);
        let mut request = make_history_request();
        request.data_type = DataType::FactorFile;

        let rows = provider.get_history(&request).await.unwrap();

        assert!(rows.is_empty());
        assert_eq!(first_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stacked_provider_caches_option_universe_by_underlying_and_date() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider =
            StackedHistoryProvider::new(vec![Arc::new(RecordingOptionUniverseProvider {
                calls: Arc::clone(&calls),
            })]);
        let date = NaiveDate::from_ymd_opt(2026, 4, 17).unwrap();

        let first = provider.get_option_universe("BE", date).await.unwrap();
        let second = provider.get_option_universe("be", date).await.unwrap();

        assert_eq!(first, second);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn default_batch_provider_skips_failed_symbol() {
        let provider = FailingSymbolProvider {
            failed_symbol: "TRVN".to_string(),
        };
        let request =
            make_history_batch_request(vec![make_symbol_for("TRVN"), make_symbol_for("SPY")]);

        let batch = provider.get_history_batch(&request).await.unwrap();

        assert_eq!(batch.trade_bars.len(), 1);
        assert_eq!(batch.trade_bars[0].symbol.value.as_ref(), "SPY");
    }

    #[tokio::test]
    async fn stacked_batch_provider_falls_back_after_provider_error() {
        let provider = StackedHistoryProvider::new(vec![
            Arc::new(MockBatchProvider {
                fail_batch: true,
                bars: vec![],
            }),
            Arc::new(MockBatchProvider {
                fail_batch: false,
                bars: vec![make_bar(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap())],
            }),
        ]);
        let request = make_history_batch_request(vec![make_symbol()]);

        let batch = provider.get_history_batch(&request).await.unwrap();

        assert_eq!(batch.trade_bars.len(), 1);
    }

    #[tokio::test]
    async fn stacked_batch_provider_returns_empty_after_all_fallbacks_fail_or_empty() {
        let provider = StackedHistoryProvider::new(vec![
            Arc::new(MockBatchProvider {
                fail_batch: true,
                bars: vec![],
            }),
            Arc::new(MockBatchProvider {
                fail_batch: false,
                bars: vec![],
            }),
        ]);
        let request = make_history_batch_request(vec![make_symbol_for("TRVN")]);

        let batch = provider.get_history_batch(&request).await.unwrap();

        assert!(batch.trade_bars.is_empty());
    }

    #[test]
    fn stacked_provider_uses_earliest_child_date() {
        let provider = StackedHistoryProvider::new(vec![
            Arc::new(MockOptionProvider {
                option_rows: vec![],
                earliest: Some(NaiveDate::from_ymd_opt(2020, 1, 1).unwrap()),
            }),
            Arc::new(MockOptionProvider {
                option_rows: vec![],
                earliest: Some(NaiveDate::from_ymd_opt(2018, 1, 1).unwrap()),
            }),
        ]);

        assert_eq!(
            provider.earliest_date(),
            Some(NaiveDate::from_ymd_opt(2018, 1, 1).unwrap())
        );
    }

    #[test]
    fn provider_config_fields() {
        let cfg = ProviderConfig {
            data_root: PathBuf::from("/data"),
            api_key: Some("key".into()),
            requests_per_second: 5.0,
            max_concurrent: 4,
        };
        assert_eq!(cfg.data_root, PathBuf::from("/data"));
        assert_eq!(cfg.api_key.as_deref(), Some("key"));
    }

    // ── LocalHistoryProvider — no data file ───────────────────────────────────

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn local_provider_returns_empty_when_no_file() {
        let Some((provider, _store)) = local_provider_with_store().await else {
            return;
        };

        let request = make_history_request();
        let bars = provider.get_history(&request).await.unwrap();

        assert!(
            bars.is_empty(),
            "Expected empty result when no Parquet file exists, got {} bars",
            bars.len()
        );
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn local_provider_returns_partial_daily_coverage() {
        let Some((provider, store)) = local_provider_with_store().await else {
            return;
        };
        write_daily_bars(
            &store,
            &[
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()),
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 5).unwrap()),
            ],
        )
        .await;

        let request = make_history_request_for_range(
            NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 5).unwrap(),
        );
        let bars = provider.get_history(&request).await.unwrap();

        assert_eq!(
            bars.len(),
            2,
            "partial local cache should return cached rows instead of forcing remote re-fetch"
        );
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn stacked_provider_uses_partial_local_without_fallback() {
        let Some((local, store)) = local_provider_with_store().await else {
            return;
        };
        write_daily_bars(
            &store,
            &[
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()),
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 5).unwrap()),
            ],
        )
        .await;

        struct RemoteOnly;
        #[async_trait::async_trait]
        impl IHistoryProvider for RemoteOnly {
            fn name(&self) -> &str {
                "remote"
            }

            async fn get_history(
                &self,
                _request: &HistoryRequest,
            ) -> anyhow::Result<Vec<TradeBar>> {
                panic!("remote provider should not be called when local cache has rows");
            }
        }

        let provider = StackedHistoryProvider::new(vec![Arc::new(local), Arc::new(RemoteOnly)]);
        let request = make_history_request_for_range(
            NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 5).unwrap(),
        );
        let bars = provider.get_history(&request).await.unwrap();
        assert_eq!(bars.len(), 2);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn local_provider_returns_data_for_complete_daily_coverage() {
        let Some((provider, store)) = local_provider_with_store().await else {
            return;
        };
        write_daily_bars(
            &store,
            &[
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()),
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 3).unwrap()),
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 4).unwrap()),
                make_bar(NaiveDate::from_ymd_opt(2024, 1, 5).unwrap()),
            ],
        )
        .await;

        let request = make_history_request_for_range(
            NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 5).unwrap(),
        );
        let bars = provider.get_history(&request).await.unwrap();

        assert_eq!(bars.len(), 4);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn local_provider_daily_coverage_uses_partition_date_not_bar_timestamp() {
        let Some((provider, store)) = local_provider_with_store().await else {
            return;
        };
        let symbol = make_symbol();
        let request_start = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let request_end = NaiveDate::from_ymd_opt(2024, 1, 5).unwrap();
        let partition_dates = [
            request_start,
            NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 4).unwrap(),
            request_end,
        ];
        let bars = partition_dates
            .into_iter()
            .map(|partition_date| make_bar_for(symbol.clone(), partition_date))
            .collect::<Vec<_>>();
        write_daily_bars(&store, &bars).await;

        let request = make_history_request_for_range(request_start, request_end);
        let bars = provider.get_history(&request).await.unwrap();

        assert_eq!(bars.len(), 4);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn local_provider_daily_coverage_accepts_start_dated_rows() {
        let Some((provider, store)) = local_provider_with_store().await else {
            return;
        };
        let symbol = make_symbol();
        let request_start = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let request_end = NaiveDate::from_ymd_opt(2024, 1, 5).unwrap();
        let bars = [
            request_start,
            NaiveDate::from_ymd_opt(2024, 1, 3).unwrap(),
            NaiveDate::from_ymd_opt(2024, 1, 4).unwrap(),
            request_end,
        ]
        .into_iter()
        .map(|date| make_start_dated_bar_for(symbol.clone(), date))
        .collect::<Vec<_>>();
        write_daily_bars(&store, &bars).await;

        let request = make_history_request_for_range(request_start, request_end);
        let bars = provider.get_history(&request).await.unwrap();

        assert_eq!(bars.len(), 4);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn local_provider_batches_option_universe_by_underlying() {
        let Some((provider, store)) = local_provider_with_store().await else {
            return;
        };
        let date = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        store
            .append_option_universe(&[
                make_option_universe_row(date, "SPY", "SPY240216C00100000"),
                make_option_universe_row(date, "QQQ", "QQQ240216C00100000"),
                make_option_universe_row(date, "AAPL", "AAPL240216C00100000"),
            ])
            .await
            .unwrap();

        let batch = provider
            .get_option_universes(&["SPY".to_string(), "AAPL".to_string()], date)
            .await
            .unwrap();

        assert_eq!(batch["SPY"].len(), 1);
        assert_eq!(batch["AAPL"].len(), 1);
        assert!(!batch.contains_key("QQQ"));
        assert_eq!(batch["SPY"][0].symbol_value, "SPY240216C00100000");
        assert_eq!(batch["AAPL"][0].symbol_value, "AAPL240216C00100000");
    }

    // ── HistoryRequest construction ───────────────────────────────────────────

    #[test]
    fn history_request_fields() {
        let req = make_history_request();
        assert_eq!(req.symbol.permtick.as_ref(), "SPY");
        assert_eq!(req.resolution, Resolution::Daily);
        assert_eq!(req.data_type, DataType::TradeBar);
    }
}
