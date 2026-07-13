use crate::data_feed::DataFeedContext;
use crate::slice_synchronizer::SliceSynchronizer;
use crate::subscription_reader::SubscriptionStream;
use chrono::NaiveDate;
use lean_core::{DateTime, LeanError, Resolution, Result as LeanResult, TickType};
use lean_data::{Slice, SubscriptionDataConfig, SubscriptionDataKind};
use lean_data_providers::ICustomDataSource;
use lean_storage::iceberg_store::{MARKET_QUOTE_BARS, MARKET_TICKS, MARKET_TRADE_BARS};
use lean_storage::IcebergStore;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tracing::debug;

/// Drives backtest data through LEAN-style subscription streams.
pub struct DataManager {
    context: DataFeedContext,
    active_subscriptions: HashMap<u64, SubscriptionDataConfig>,
    synchronizer: Option<SliceSynchronizer>,
    end: Option<DateTime>,
    last_market_cache_flush_date: Option<NaiveDate>,
}

impl DataManager {
    pub fn from_store(store: Arc<IcebergStore>) -> Self {
        Self::from_context(DataFeedContext::new(store))
    }

    pub fn with_custom_data_sources_from_store(
        store: Arc<IcebergStore>,
        custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
    ) -> Self {
        Self::from_context(
            DataFeedContext::new(store).with_custom_data_sources(custom_data_sources),
        )
    }

    pub fn from_context(context: DataFeedContext) -> Self {
        DataManager {
            context,
            active_subscriptions: HashMap::new(),
            synchronizer: None,
            end: None,
            last_market_cache_flush_date: None,
        }
    }

    pub fn context(&self) -> &DataFeedContext {
        &self.context
    }

    /// Initialize subscription streams for the backtest period.
    pub async fn initialize_feed(
        &mut self,
        configs: &[SubscriptionDataConfig],
        start: DateTime,
        end: DateTime,
    ) -> LeanResult<()> {
        self.warm_market_partition_indexes(configs).await?;
        self.warm_custom_partition_index_if_needed(configs).await?;
        self.active_subscriptions.clear();
        self.last_market_cache_flush_date = None;
        self.context.seed_consumer_frontier(start.date_utc());
        let mut streams = Vec::with_capacity(configs.len());
        for config in configs.iter().cloned() {
            self.active_subscriptions
                .insert(config.unique_id(), config.clone());
            streams.push(SubscriptionStream::new(
                config,
                self.context.clone(),
                start,
                end,
            ));
        }
        self.synchronizer = Some(SliceSynchronizer::new(streams, end, self.context.clone()));
        self.end = Some(end);
        self.sync_subscription_cache_policy();
        debug!(
            "Initialized subscription feed with {} stream(s)",
            self.synchronizer
                .as_ref()
                .map(|s| s.streams().len())
                .unwrap_or(0)
        );
        Ok(())
    }

    fn sync_subscription_cache_policy(&mut self) {
        self.context
            .active_subscription_count
            .store(self.active_subscriptions.len(), Ordering::Relaxed);
    }

    async fn warm_market_partition_indexes(
        &self,
        configs: &[SubscriptionDataConfig],
    ) -> LeanResult<()> {
        let tables = market_partition_tables(configs);
        if tables.is_empty() {
            return Ok(());
        }
        self.context
            .store
            .warm_market_partition_indexes(tables)
            .await
            .map_err(|error| LeanError::DataError(error.to_string()))
    }

    async fn warm_custom_partition_index_if_needed(
        &self,
        configs: &[SubscriptionDataConfig],
    ) -> LeanResult<()> {
        if !configs.iter().any(|config| config.custom.is_some()) {
            return Ok(());
        }
        self.context
            .store
            .warm_custom_partition_index()
            .await
            .map_err(|error| LeanError::DataError(error.to_string()))
    }

    pub fn add_subscription(&mut self, config: SubscriptionDataConfig, start: DateTime) {
        let id = config.unique_id();
        if self.active_subscriptions.contains_key(&id) {
            return;
        }
        let end = self.end.unwrap_or(start);
        let stream = SubscriptionStream::new(config.clone(), self.context.clone(), start, end);
        self.active_subscriptions.insert(id, config);
        self.sync_subscription_cache_policy();
        match self.synchronizer.as_mut() {
            Some(sync) => sync.add_stream(stream),
            None => {
                self.synchronizer = Some(SliceSynchronizer::new(
                    vec![stream],
                    end,
                    self.context.clone(),
                ))
            }
        }
    }

    pub async fn add_subscription_async(
        &mut self,
        config: SubscriptionDataConfig,
        start: DateTime,
    ) -> LeanResult<()> {
        self.warm_market_partition_indexes(std::slice::from_ref(&config))
            .await?;
        self.add_subscription(config, start);
        Ok(())
    }

    pub async fn add_subscriptions_async(
        &mut self,
        configs: Vec<SubscriptionDataConfig>,
        start: DateTime,
    ) -> LeanResult<()> {
        let new_configs = configs
            .into_iter()
            .filter(|config| !self.active_subscriptions.contains_key(&config.unique_id()))
            .collect::<Vec<_>>();
        if new_configs.is_empty() {
            return Ok(());
        }
        self.warm_market_partition_indexes(&new_configs).await?;
        self.warm_custom_partition_index_if_needed(&new_configs)
            .await?;
        let end = self.end.unwrap_or(start);
        let mut streams = Vec::with_capacity(new_configs.len());
        for config in new_configs {
            let id = config.unique_id();
            streams.push(SubscriptionStream::new(
                config.clone(),
                self.context.clone(),
                start,
                end,
            ));
            self.active_subscriptions.insert(id, config);
        }
        match self.synchronizer.as_mut() {
            Some(sync) => {
                for stream in streams {
                    sync.add_stream(stream);
                }
            }
            None => {
                self.synchronizer = Some(SliceSynchronizer::new(streams, end, self.context.clone()))
            }
        }
        self.sync_subscription_cache_policy();
        Ok(())
    }

    pub fn remove_subscription(&mut self, config: &SubscriptionDataConfig) {
        let id = config.unique_id();
        if self.active_subscriptions.remove(&id).is_some() {
            if let Some(sync) = self.synchronizer.as_mut() {
                sync.remove_stream(id);
            }
            self.sync_subscription_cache_policy();
        }
    }

    /// Advance to the next synchronized slice across all subscriptions.
    pub async fn next_slice(&mut self) -> LeanResult<Option<Slice>> {
        let slice = match self.synchronizer.as_mut() {
            Some(sync) => sync.next_slice().await?,
            None => None,
        };
        // Publish the consumer frontier so producers can gate how far ahead they
        // load/fetch. Without this, every producer races to the backtest end,
        // fetching months of not-yet-needed data on large universes.
        if let Some(slice) = slice.as_ref() {
            let date = slice.time.date_utc();
            self.context.observe_consumer_frontier(date);
            if self.last_market_cache_flush_date != Some(date) {
                self.context
                    .flush_market_cache_writes_through(date)
                    .await
                    .map_err(|error| LeanError::DataError(error.to_string()))?;
                self.last_market_cache_flush_date = Some(date);
            }
        }
        Ok(slice)
    }

    pub fn store(&self) -> &IcebergStore {
        &self.context.store
    }
}

fn market_partition_tables(configs: &[SubscriptionDataConfig]) -> Vec<&'static str> {
    let mut tables = HashSet::new();
    for config in configs {
        if config.data_kind != SubscriptionDataKind::Market
            || config.resolution == Resolution::Daily
        {
            continue;
        }
        let table = if config.resolution.is_tick() {
            MARKET_TICKS
        } else if config.tick_type == TickType::Quote {
            MARKET_QUOTE_BARS
        } else {
            MARKET_TRADE_BARS
        };
        tables.insert(table);
    }
    tables.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::DataManager;
    use async_trait::async_trait;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{
        DataNormalizationMode, DateTime, Market, OptionRight, OptionStyle, Resolution, Symbol,
        SymbolOptionsExt, TickType, TimeSpan,
    };
    use lean_data::{
        CustomDataConfig, CustomDataFormat, CustomDataPoint, CustomDataQuery, CustomDataSource,
        CustomDataTransport, CustomSubscriptionMetadata, OptionChainFilterMetadata,
        OptionChainSubscriptionMetadata, SubscriptionDataConfig, Tick, TradeBar, TradeBarData,
    };
    use lean_data_providers::{
        CustomDataContext, HistoryRequest, ICustomDataSource, IHistoryProvider,
        StackedHistoryProvider,
    };
    use lean_storage::{FactorFileEntry, OptionEodBar};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_skips_missing_daily_partitions_without_error() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2018, 6, 18).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2018, 6, 19).unwrap();
        let day3 = NaiveDate::from_ymd_opt(2018, 6, 20).unwrap();

        let bar = TradeBar::new(
            symbol.clone(),
            dt(day2, 16, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(50), dec!(50), dec!(50), dec!(50), dec!(1000)),
        );

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Adjusted,
        );
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &[bar],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
        manager
            .initialize_feed(&[config], dt(day1, 0, 0), dt(day3, 23, 59))
            .await
            .unwrap();

        let mut emitted = 0;
        while let Some(slice) = manager.next_slice().await.unwrap() {
            assert!(slice.get_bar(&symbol).is_some());
            emitted += 1;
        }
        assert_eq!(emitted, 1);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_emits_cross_midnight_daily_bar_on_session_frontier() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let day0 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();

        let bar = TradeBar::new(
            symbol.clone(),
            dt(day1, 20, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Adjusted,
        );
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &[bar],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
        manager
            .store()
            .append_factor_file(
                "usa",
                "SPY",
                &[
                    FactorFileEntry {
                        date: day0,
                        price_factor: 1.0,
                        split_factor: 1.0,
                        reference_price: 0.0,
                    },
                    FactorFileEntry {
                        date: day1,
                        price_factor: 2.0,
                        split_factor: 1.0,
                        reference_price: 0.0,
                    },
                    FactorFileEntry {
                        date: day2,
                        price_factor: 4.0,
                        split_factor: 1.0,
                        reference_price: 0.0,
                    },
                ],
            )
            .await
            .unwrap();
        manager
            .initialize_feed(&[config], dt(day2, 0, 0), dt(day2, 23, 59))
            .await
            .unwrap();

        let slice = manager.next_slice().await.unwrap().expect("daily bar");
        let emitted = slice.get_bar(&symbol).expect("spy bar");
        assert_eq!(emitted.end_time.date_utc(), day2);
        // Frontier is day2, so factor row before day2 (day1 @ 2.0) applies — not day0 @ 1.0 from bar.time date.
        assert_eq!(emitted.close, dec!(200));
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_emits_prefetched_daily_option_chain_with_greeks() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let expiry = NaiveDate::from_ymd_opt(2024, 2, 16).unwrap();

        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &[TradeBar::new(
                    underlying.clone(),
                    dt(day, 16, 0),
                    TimeSpan::ONE_DAY,
                    TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
                )],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
        manager
            .store()
            .append_option_eod_bars(&[
                OptionEodBar {
                    date: day,
                    symbol_value: "SPY240216C00100000".to_string(),
                    underlying: "SPY".to_string(),
                    expiration: expiry,
                    strike: dec!(100),
                    right: "C".to_string(),
                    open: dec!(5),
                    high: dec!(6),
                    low: dec!(4),
                    close: dec!(5),
                    volume: 10,
                    bid: dec!(4.9),
                    ask: dec!(5.1),
                    bid_size: 1,
                    ask_size: 1,
                },
                OptionEodBar {
                    date: day,
                    symbol_value: "SPY240216P00100000".to_string(),
                    underlying: "SPY".to_string(),
                    expiration: expiry,
                    strike: dec!(100),
                    right: "P".to_string(),
                    open: dec!(5),
                    high: dec!(6),
                    low: dec!(4),
                    close: dec!(5),
                    volume: 10,
                    bid: dec!(4.9),
                    ask: dec!(5.1),
                    bid_size: 1,
                    ask_size: 1,
                },
            ])
            .await
            .unwrap();

        let option_config = SubscriptionDataConfig::new_option_chain(
            canonical.clone(),
            Resolution::Daily,
            OptionChainSubscriptionMetadata {
                canonical_permtick: canonical.permtick.to_string(),
                underlying_ticker: "SPY".to_string(),
                filter: OptionChainFilterMetadata {
                    min_strike_rank: -1,
                    max_strike_rank: 1,
                    min_expiry_days: 0,
                    max_expiry_days: 60,
                },
            },
        );
        manager
            .initialize_feed(&[option_config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let slice = manager
            .next_slice()
            .await
            .unwrap()
            .expect("option chain slice");
        let chain = slice
            .option_chains
            .get(canonical.permtick.as_ref())
            .expect("chain for canonical");
        assert_eq!(chain.contracts.len(), 2);
        assert!(chain
            .contracts
            .values()
            .any(|contract| contract.right == OptionRight::Call
                && contract.style == OptionStyle::American
                && contract.data.underlying_last_price == dec!(100)
                && contract.data.implied_volatility > dec!(0)));
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_emits_all_hourly_bars_in_partition() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                dt(day, 15, 0),
                TimeSpan::ONE_HOUR,
                TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(day, 16, 0),
                TimeSpan::ONE_HOUR,
                TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(2), dec!(100)),
            ),
        ];

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Hour,
            DataNormalizationMode::Raw,
        );
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &bars,
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Hour,
                TickType::Trade,
            )
            .await
            .unwrap();
        manager
            .initialize_feed(
                &[config],
                dt(day, 0, 0),
                dt(day + chrono::Duration::days(1), 23, 59),
            )
            .await
            .unwrap();

        let mut closes = Vec::new();
        while let Some(slice) = manager.next_slice().await.unwrap() {
            if let Some(bar) = slice.get_bar(&symbol) {
                closes.push(bar.close);
            }
        }
        assert_eq!(closes, vec![dec!(1), dec!(2)]);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_emits_tick_partition_rows() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let ticks = vec![
            Tick::trade(symbol.clone(), dt(day, 9, 31), dec!(1), dec!(100)),
            Tick::trade(symbol.clone(), dt(day, 9, 32), dec!(2), dec!(100)),
        ];

        let mut config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Tick,
            DataNormalizationMode::Raw,
        );
        config.fill_data_forward = false;
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_ticks(
                &ticks,
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Tick,
                TickType::Trade,
            )
            .await
            .unwrap();
        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let mut values = Vec::new();
        while let Some(slice) = manager.next_slice().await.unwrap() {
            if let Some(ticks) = slice.get_ticks(&symbol) {
                values.extend(ticks.iter().map(|tick| tick.value));
            }
        }
        assert_eq!(values, vec![dec!(1), dec!(2)]);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_emits_custom_data_from_subscription_config() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_base("fixture", "ALT", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_custom_points(
                "fixture",
                "ALT",
                &[CustomDataPoint::empty(day, Some(dt(day, 16, 0)), dec!(42))],
            )
            .await
            .unwrap();

        let custom_config = CustomDataConfig {
            ticker: "ALT".to_string(),
            source_type: "fixture".to_string(),
            resolution: Resolution::Daily,
            properties: std::collections::HashMap::new(),
            query: CustomDataQuery::default(),
        };
        let metadata = CustomSubscriptionMetadata {
            source_type: "fixture".to_string(),
            ticker: "ALT".to_string(),
            config: custom_config,
            dynamic_query: CustomDataQuery::default(),
        };
        let config =
            SubscriptionDataConfig::new_custom(symbol.clone(), Resolution::Daily, metadata);
        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let slice = manager.next_slice().await.unwrap().expect("custom slice");
        let points = slice.get_custom_data(&symbol).expect("custom data");
        assert_eq!(points[0].value, dec!(42));
        assert_eq!(slice.custom_data["ALT"][0].value, dec!(42));
    }

    struct FixtureCustomSource {
        path: std::path::PathBuf,
    }

    impl ICustomDataSource for FixtureCustomSource {
        fn initialize(&mut self, _context: &CustomDataContext) {}

        fn name(&self) -> &str {
            "fixture"
        }

        fn get_source(
            &self,
            _ticker: &str,
            _date: NaiveDate,
            _config: &CustomDataConfig,
        ) -> Option<CustomDataSource> {
            Some(CustomDataSource {
                uri: self.path.to_string_lossy().into_owned(),
                transport: CustomDataTransport::LocalFile,
                format: CustomDataFormat::Csv,
                headers: HashMap::new(),
                symbol_column: None,
            })
        }

        fn reader(
            &self,
            _line: &str,
            date: NaiveDate,
            _config: &CustomDataConfig,
        ) -> Option<CustomDataPoint> {
            Some(CustomDataPoint::empty(date, Some(dt(date, 16, 0)), dec!(7)))
        }
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_fetches_custom_data_on_cache_miss() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let symbol = Symbol::create_base("fixture", "ALT", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2026, 3, 26).unwrap();
        let custom_config = CustomDataConfig {
            ticker: "ALT".to_string(),
            source_type: "fixture".to_string(),
            resolution: Resolution::Daily,
            properties: std::collections::HashMap::new(),
            query: CustomDataQuery::default(),
        };
        let metadata = CustomSubscriptionMetadata {
            source_type: "fixture".to_string(),
            ticker: "ALT".to_string(),
            config: custom_config,
            dynamic_query: CustomDataQuery::default(),
        };
        let config =
            SubscriptionDataConfig::new_custom(symbol.clone(), Resolution::Daily, metadata);
        let fixture_path = tmp.path().join("fixture.csv");
        std::fs::write(&fixture_path, "row\n").unwrap();
        let mut manager = DataManager::with_custom_data_sources_from_store(
            store,
            vec![Arc::new(FixtureCustomSource { path: fixture_path })],
        );

        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let slice = manager.next_slice().await.unwrap().expect("custom slice");
        let points = slice.get_custom_data(&symbol).expect("custom data");
        assert_eq!(points[0].value, dec!(7));
    }

    struct SourceDatedDailyHistoryProvider;

    #[async_trait]
    impl IHistoryProvider for SourceDatedDailyHistoryProvider {
        async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
            let day = request.start.date_utc();
            Ok(vec![TradeBar::new(
                request.symbol.clone(),
                dt(day - chrono::Duration::days(1), 20, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(11), dec!(11), dec!(11), dec!(11), dec!(100)),
            )])
        }
    }

    struct RecordingHistoryProvider {
        name: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        bars: Vec<TradeBar>,
    }

    #[async_trait]
    impl IHistoryProvider for RecordingHistoryProvider {
        async fn get_history(&self, _request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
            self.calls.lock().unwrap().push(self.name);
            Ok(self.bars.clone())
        }
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_fetches_market_data_on_cache_miss() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let context = crate::data_feed::DataFeedContext::new(store)
            .with_history_provider(Some(Arc::new(SourceDatedDailyHistoryProvider)));
        let mut manager = DataManager::from_context(context);

        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let slice = manager.next_slice().await.unwrap().expect("market slice");
        let bar = slice.get_bar(&symbol).expect("fetched bar");
        assert_eq!(bar.close, dec!(11));
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_uses_local_cache_before_stacked_history_providers() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let local_bar = TradeBar::new(
            symbol.clone(),
            dt(day - chrono::Duration::days(1), 20, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(101), dec!(101), dec!(101), dec!(101), dec!(100)),
        );
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        store
            .append_trade_bars(
                &[local_bar],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(StackedHistoryProvider::new(vec![
            Arc::new(RecordingHistoryProvider {
                name: "first",
                calls: calls.clone(),
                bars: Vec::new(),
            }),
            Arc::new(RecordingHistoryProvider {
                name: "second",
                calls: calls.clone(),
                bars: Vec::new(),
            }),
        ]));
        let context =
            crate::data_feed::DataFeedContext::new(store).with_history_provider(Some(provider));
        let mut manager = DataManager::from_context(context);

        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let slice = manager.next_slice().await.unwrap().expect("market slice");
        let bar = slice.get_bar(&symbol).expect("local bar");
        assert_eq!(bar.close, dec!(101));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_falls_through_stacked_history_providers_on_cache_miss() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let remote_bar = TradeBar::new(
            symbol.clone(),
            dt(day - chrono::Duration::days(1), 20, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(202), dec!(202), dec!(202), dec!(202), dec!(100)),
        );
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(StackedHistoryProvider::new(vec![
            Arc::new(RecordingHistoryProvider {
                name: "first",
                calls: calls.clone(),
                bars: Vec::new(),
            }),
            Arc::new(RecordingHistoryProvider {
                name: "second",
                calls: calls.clone(),
                bars: vec![remote_bar],
            }),
        ]));
        let context =
            crate::data_feed::DataFeedContext::new(store).with_history_provider(Some(provider));
        let mut manager = DataManager::from_context(context);

        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
            .await
            .unwrap();

        let slice = manager.next_slice().await.unwrap().expect("market slice");
        let bar = slice.get_bar(&symbol).expect("fetched bar");
        assert_eq!(bar.close, dec!(202));
        assert_eq!(calls.lock().unwrap().as_slice(), &["first", "second"]);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn feed_fetches_source_dated_daily_market_data_across_multiple_cache_misses() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let start_day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let end_day = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let context = crate::data_feed::DataFeedContext::new(store)
            .with_history_provider(Some(Arc::new(SourceDatedDailyHistoryProvider)));
        let mut manager = DataManager::from_context(context);

        manager
            .initialize_feed(&[config], dt(start_day, 0, 0), dt(end_day, 23, 59))
            .await
            .unwrap();

        let mut closes = Vec::new();
        while let Some(slice) = manager.next_slice().await.unwrap() {
            if let Some(bar) = slice.get_bar(&symbol) {
                closes.push(bar.close);
            }
        }

        assert_eq!(closes, vec![dec!(11), dec!(11), dec!(11), dec!(11)]);
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn add_subscription_emits_only_from_add_time() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let qqq = Symbol::create_equity("QQQ", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let spy_bar = TradeBar::new(
            spy.clone(),
            dt(day + chrono::Duration::days(1), 16, 0),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
        );
        let qqq_before_add = TradeBar::new(
            qqq.clone(),
            dt(day + chrono::Duration::days(1), 16, 0),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(2), dec!(100)),
        );
        let qqq_after_add = TradeBar::new(
            qqq.clone(),
            dt(day + chrono::Duration::days(2), 16, 0),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(3), dec!(3), dec!(3), dec!(3), dec!(100)),
        );
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &[spy_bar, qqq_before_add, qqq_after_add],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Minute,
                TickType::Trade,
            )
            .await
            .unwrap();
        let spy_config = SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let qqq_config = SubscriptionDataConfig::new_equity(
            qqq.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        manager
            .initialize_feed(
                &[spy_config],
                dt(day + chrono::Duration::days(1), 0, 0),
                dt(day + chrono::Duration::days(2), 23, 59),
            )
            .await
            .unwrap();

        let first = manager.next_slice().await.unwrap().expect("spy slice");
        assert_eq!(first.get_bar(&spy).expect("spy after start").close, dec!(1));
        assert!(first.get_bar(&qqq).is_none());

        manager.add_subscription(qqq_config, dt(day + chrono::Duration::days(2), 0, 0));
        let second = manager.next_slice().await.unwrap().expect("qqq slice");
        assert_eq!(second.get_bar(&qqq).expect("qqq after add").close, dec!(3));
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn empty_minute_custom_subscription_does_not_block_market_slice() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let bar = TradeBar::new(
            spy.clone(),
            dt(day, 14, 35),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
        );
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &[bar],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Minute,
                TickType::Trade,
            )
            .await
            .unwrap();

        let custom_symbol = Symbol::create_base("tradealert", "sweeps", &Market::usa());
        let custom_metadata = CustomSubscriptionMetadata {
            source_type: "tradealert".to_string(),
            ticker: "sweeps".to_string(),
            config: CustomDataConfig {
                ticker: "sweeps".to_string(),
                source_type: "tradealert".to_string(),
                resolution: Resolution::Minute,
                properties: HashMap::new(),
                query: CustomDataQuery::default(),
            },
            dynamic_query: CustomDataQuery {
                symbols: Some(Vec::new()),
                ..CustomDataQuery::default()
            },
        };
        let market_config = SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let custom_config =
            SubscriptionDataConfig::new_custom(custom_symbol, Resolution::Minute, custom_metadata);

        manager
            .initialize_feed(
                &[market_config, custom_config],
                dt(day, 0, 0),
                dt(day, 23, 59),
            )
            .await
            .unwrap();

        let slice = tokio::time::timeout(std::time::Duration::from_secs(2), manager.next_slice())
            .await
            .expect("empty custom stream should not block synchronizer")
            .unwrap()
            .expect("market slice");
        assert_eq!(slice.get_bar(&spy).expect("spy bar").close, dec!(1));
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn unset_minute_custom_subscription_does_not_block_market_slice() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let bar = TradeBar::new(
            spy.clone(),
            dt(day, 14, 35),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
        );
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &[bar],
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Minute,
                TickType::Trade,
            )
            .await
            .unwrap();

        let custom_symbol = Symbol::create_base("standalone", "custom", &Market::usa());
        let custom_metadata = CustomSubscriptionMetadata {
            source_type: "standalone".to_string(),
            ticker: "custom".to_string(),
            config: CustomDataConfig {
                ticker: "custom".to_string(),
                source_type: "standalone".to_string(),
                resolution: Resolution::Minute,
                properties: HashMap::new(),
                query: CustomDataQuery::default(),
            },
            dynamic_query: CustomDataQuery::default(),
        };
        let market_config = SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let custom_config =
            SubscriptionDataConfig::new_custom(custom_symbol, Resolution::Minute, custom_metadata);

        manager
            .initialize_feed(
                &[market_config, custom_config],
                dt(day, 0, 0),
                dt(day, 23, 59),
            )
            .await
            .unwrap();

        let slice = tokio::time::timeout(std::time::Duration::from_secs(2), manager.next_slice())
            .await
            .expect("unset empty custom stream should not block synchronizer")
            .unwrap()
            .expect("market slice");
        assert_eq!(slice.get_bar(&spy).expect("spy bar").close, dec!(1));
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn remove_subscription_stops_future_emissions() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let bars = vec![
            TradeBar::new(
                spy.clone(),
                dt(day, 0, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
            ),
            TradeBar::new(
                spy.clone(),
                dt(day + chrono::Duration::days(1), 0, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(2), dec!(100)),
            ),
        ];
        let mut manager = DataManager::from_store(store);
        manager
            .store()
            .append_trade_bars(
                &bars,
                lean_core::SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();
        let config = SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        manager
            .initialize_feed(
                std::slice::from_ref(&config),
                dt(day + chrono::Duration::days(1), 0, 0),
                dt(day + chrono::Duration::days(2), 23, 59),
            )
            .await
            .unwrap();

        let first = manager.next_slice().await.unwrap().expect("first slice");
        assert_eq!(first.get_bar(&spy).expect("first bar").close, dec!(2));
        manager.remove_subscription(&config);
        assert!(manager.next_slice().await.unwrap().is_none());
    }
}
