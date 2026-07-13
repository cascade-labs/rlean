use crate::data_feed::DataFeedContext;
use crate::history_subscription::SubscriptionHistoryProvider;
use anyhow::{anyhow, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use lean_algorithm::lifecycle::{AlgorithmHistoryService, HistoryColumns};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_core::{DataNormalizationMode, DateTime, NanosecondTimestamp, Resolution, Symbol};
use lean_data::{CustomDataPoint, SubscriptionDataConfig, TradeBar};
use lean_data_providers::{DataType, HistoryRequest, ICustomDataSource, IHistoryProvider};
use lean_storage::IcebergStore;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AlgorithmHistoryContext {
    pub data_root: PathBuf,
    pub data_store: Arc<IcebergStore>,
    pub history_provider: Option<Arc<dyn IHistoryProvider>>,
    pub custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
}

#[derive(Clone)]
pub struct HistoryService {
    context: AlgorithmHistoryContext,
}

impl HistoryService {
    pub fn new(context: AlgorithmHistoryContext) -> Self {
        Self { context }
    }

    pub fn load_trade_bars_blocking_with_normalization(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        self.load_trade_bars_between_blocking_with_normalization(
            symbol,
            resolution,
            date_to_datetime(start, 0, 0, 0),
            date_to_datetime(end, 23, 59, 59),
            normalization_mode,
        )
    }

    pub fn load_trade_bars_between_blocking_with_normalization(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: DateTime,
        end: DateTime,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        let request = HistoryRequest {
            symbol: symbol.clone(),
            resolution,
            start,
            end,
            data_type: DataType::TradeBar,
        };

        let provider = self.subscription_history_provider();
        let bars = block_on_background(async move {
            provider
                .get_trade_bars(&request, normalization_mode)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })?;
        Ok(bars)
    }

    /// Load the single most-recent known trade bar for `symbol`, applying
    /// LEAN's resolution-dependent lookback window. Language-neutral rule moved
    /// out of the Python binding.
    pub fn load_last_known_trade_bar(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        as_of: DateTime,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        let lookback_days = last_known_lookback_days(resolution);
        let end_date = as_of.date_utc();
        let start_date = end_date - chrono::Duration::days(lookback_days);
        let start = date_to_datetime(start_date, 0, 0, 0);
        let mut bars = self.load_trade_bars_between_blocking_with_normalization(
            symbol,
            resolution,
            start,
            as_of,
            normalization_mode,
        )?;
        bars.retain(|bar| bar.close > Decimal::ZERO && bar.end_time.0 <= as_of.0);
        bars.sort_by_key(|bar| bar.end_time.0);
        if bars.len() > 1 {
            bars = bars[bars.len() - 1..].to_vec();
        }
        Ok(bars)
    }

    pub fn load_custom_history_blocking(
        &self,
        subscription: &SubscriptionDataConfig,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<CustomDataPoint>> {
        subscription
            .custom
            .as_ref()
            .ok_or_else(|| anyhow!("custom history requested for non-custom subscription"))?;
        let provider = self.subscription_history_provider();
        let config = subscription.clone();
        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt = date_to_datetime(end, 23, 59, 59);
        block_on_background(async move {
            provider
                .get_custom_points(config, start_dt, end_dt)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })
    }

    fn subscription_history_provider(&self) -> SubscriptionHistoryProvider {
        let mut context = DataFeedContext::new(self.context.data_store.clone())
            .with_custom_data_sources(self.context.custom_data_sources.clone());
        context = context.with_history_provider(self.context.history_provider.clone());
        SubscriptionHistoryProvider::new(context)
    }
}

impl AlgorithmHistoryService for HistoryService {
    fn history(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        periods: usize,
        resolution: Resolution,
    ) -> HistoryColumns {
        self.history_count_columns(algorithm, symbol, periods, resolution)
            .unwrap_or_default()
    }

    fn last_known_close_price(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        resolution: Resolution,
    ) -> Option<f64> {
        let as_of = if algorithm.utc_time == DateTime::EPOCH {
            algorithm.start_date
        } else {
            algorithm.utc_time
        };
        let configs = algorithm
            .subscription_manager
            .get_configs_for_symbol(symbol);
        let normalization = matching_normalization_mode(
            &configs
                .iter()
                .map(|config| (**config).clone())
                .collect::<Vec<_>>(),
            Some(resolution),
            DataNormalizationMode::Adjusted,
        );
        let bars = self
            .load_last_known_trade_bar(symbol, resolution, as_of, normalization)
            .ok()?;
        bars.last()
            .and_then(|bar| bar.close.to_f64())
            .filter(|price| *price > 0.0 && price.is_finite())
    }
}

impl HistoryService {
    fn history_count_columns(
        &self,
        algorithm: &QcAlgorithm,
        symbol: &Symbol,
        periods: usize,
        resolution: Resolution,
    ) -> Result<HistoryColumns> {
        if periods == 0 {
            return Ok(HistoryColumns::new());
        }

        let end = if algorithm.utc_time == DateTime::EPOCH {
            algorithm.start_date
        } else {
            algorithm.utc_time
        };
        let start = history_count_start(end, periods, resolution);
        let configs = algorithm
            .subscription_manager
            .get_configs_for_symbol(symbol);
        if let Some(custom_config) = configs.iter().find(|config| config.custom.is_some()) {
            let mut points =
                self.load_custom_history_between_blocking(custom_config, start, end)?;
            points.sort_by_key(|point| point.end_time.map(|time| time.0).unwrap_or_default());
            if points.len() > periods {
                points = points[points.len() - periods..].to_vec();
            }
            Ok(custom_points_to_columns(&points))
        } else {
            let normalization = matching_normalization_mode(
                &configs
                    .iter()
                    .map(|config| (**config).clone())
                    .collect::<Vec<_>>(),
                Some(resolution),
                DataNormalizationMode::Adjusted,
            );
            let mut bars = self.load_trade_bars_between_blocking_with_normalization(
                symbol,
                resolution,
                start,
                end,
                normalization,
            )?;
            bars.sort_by_key(|bar| bar.end_time.0);
            if bars.len() > periods {
                bars = bars[bars.len() - periods..].to_vec();
            }
            Ok(trade_bars_to_columns(&bars))
        }
    }

    fn load_custom_history_between_blocking(
        &self,
        subscription: &SubscriptionDataConfig,
        start: DateTime,
        end: DateTime,
    ) -> Result<Vec<CustomDataPoint>> {
        subscription
            .custom
            .as_ref()
            .ok_or_else(|| anyhow!("custom history requested for non-custom subscription"))?;
        let provider = self.subscription_history_provider();
        let config = subscription.clone();
        block_on_background(async move {
            provider
                .get_custom_points(config, start, end)
                .await
                .map_err(|error| anyhow!(error.to_string()))
        })
    }
}

fn history_count_start(end: DateTime, periods: usize, resolution: Resolution) -> DateTime {
    let calendar_days = match resolution {
        Resolution::Daily => periods.saturating_mul(3).saturating_add(31),
        Resolution::Hour => periods.saturating_div(6).saturating_add(14),
        Resolution::Minute | Resolution::Second | Resolution::Tick => {
            periods.saturating_div(390).saturating_add(7)
        }
    };
    NanosecondTimestamp(
        end.0.saturating_sub(
            chrono::Duration::days(calendar_days as i64)
                .num_nanoseconds()
                .unwrap_or(i64::MAX),
        ),
    )
}

fn trade_bars_to_columns(bars: &[TradeBar]) -> HistoryColumns {
    let mut columns = HistoryColumns::new();
    columns.insert("time".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("end_time".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("open".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("high".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("low".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("close".to_string(), Vec::with_capacity(bars.len()));
    columns.insert("volume".to_string(), Vec::with_capacity(bars.len()));
    for bar in bars {
        columns
            .get_mut("time")
            .unwrap()
            .push(bar.time.to_utc().to_rfc3339());
        columns
            .get_mut("end_time")
            .unwrap()
            .push(bar.end_time.to_utc().to_rfc3339());
        columns.get_mut("open").unwrap().push(bar.open.to_string());
        columns.get_mut("high").unwrap().push(bar.high.to_string());
        columns.get_mut("low").unwrap().push(bar.low.to_string());
        columns
            .get_mut("close")
            .unwrap()
            .push(bar.close.to_string());
        columns
            .get_mut("volume")
            .unwrap()
            .push(bar.volume.to_string());
    }
    columns
}

fn custom_points_to_columns(points: &[CustomDataPoint]) -> HistoryColumns {
    let mut columns = HistoryColumns::new();
    columns.insert("time".to_string(), Vec::with_capacity(points.len()));
    columns.insert("end_time".to_string(), Vec::with_capacity(points.len()));
    columns.insert("value".to_string(), Vec::with_capacity(points.len()));
    for point in points {
        columns
            .get_mut("time")
            .unwrap()
            .push(point.time.to_string());
        columns.get_mut("end_time").unwrap().push(
            point
                .end_time
                .map(|time| time.to_utc().to_rfc3339())
                .unwrap_or_else(|| point.time.to_string()),
        );
        columns
            .get_mut("value")
            .unwrap()
            .push(point.value.to_string());
    }
    columns
}

/// LEAN's resolution-dependent lookback window for "last known price" lookups.
pub fn last_known_lookback_days(resolution: Resolution) -> i64 {
    match resolution {
        Resolution::Tick | Resolution::Second | Resolution::Minute => 7,
        Resolution::Hour => 14,
        Resolution::Daily => 31,
    }
}

/// Select the data-normalization mode for a history request from the matching
/// subscriptions, mirroring C# Lean's `GetMatchingSubscriptions`: prefer a
/// trade subscription at the requested resolution, then any subscription for
/// the symbol, finally fall back to `fallback` (the universe-settings default).
pub fn matching_normalization_mode(
    configs: &[SubscriptionDataConfig],
    resolution: Option<Resolution>,
    fallback: DataNormalizationMode,
) -> DataNormalizationMode {
    use lean_core::TickType;
    if let Some(resolution) = resolution {
        if let Some(sub) = configs
            .iter()
            .find(|sub| sub.resolution == resolution && sub.tick_type == TickType::Trade)
        {
            return sub.normalization_mode;
        }
        if let Some(sub) = configs.iter().find(|sub| sub.resolution == resolution) {
            return sub.normalization_mode;
        }
    }
    if let Some(sub) = configs.iter().find(|sub| sub.tick_type == TickType::Trade) {
        return sub.normalization_mode;
    }
    if let Some(sub) = configs.first() {
        return sub.normalization_mode;
    }
    fallback
}

fn date_to_datetime(date: NaiveDate, hour: u32, minute: u32, second: u32) -> DateTime {
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, second).unwrap()))
}

fn block_on_background<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let handle = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow!(e))?
            .block_on(future)
    });
    handle
        .join()
        .map_err(|_| anyhow!("history worker panicked"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, TimeSpan};
    use lean_data::{
        CustomDataConfig, CustomDataFormat, CustomDataQuery, CustomDataSource, CustomDataTransport,
        CustomSubscriptionMetadata, TradeBar, TradeBarData,
    };
    use lean_data_providers::{CustomDataContext, ICustomDataSource};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        date_to_datetime(date, hour, minute, 0)
    }

    fn context(store: Arc<IcebergStore>) -> AlgorithmHistoryContext {
        AlgorithmHistoryContext {
            data_root: PathBuf::new(),
            data_store: store,
            history_provider: None,
            custom_data_sources: Vec::new(),
        }
    }

    #[test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    fn history_uses_subscription_daily_frontier_semantics() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let Some(store) = rt.block_on(crate::test_support::connect_test_store()) else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let start_day = day(2024, 1, 16);
        let end_day = day(2024, 1, 17);
        let bar = TradeBar::new(
            symbol.clone(),
            dt(start_day, 20, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100), dec!(1000)),
        );
        rt.block_on(async {
            store
                .append_trade_bars(
                    &[bar],
                    symbol.security_type(),
                    symbol.market().as_str(),
                    Resolution::Daily,
                    lean_core::TickType::Trade,
                )
                .await
        })
        .unwrap();

        let service = HistoryService::new(context(store));
        let bars = service
            .load_trade_bars_between_blocking_with_normalization(
                &symbol,
                Resolution::Daily,
                dt(end_day, 0, 0),
                dt(end_day, 23, 59),
                DataNormalizationMode::Raw,
            )
            .unwrap();

        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].time.date_utc(), start_day);
        assert_eq!(bars[0].end_time.date_utc(), end_day);
    }

    #[test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    fn last_known_price_uses_subscription_history() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let Some(store) = rt.block_on(crate::test_support::connect_test_store()) else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let first_day = day(2024, 1, 16);
        let second_day = day(2024, 1, 17);
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                dt(first_day - chrono::Duration::days(1), 20, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(second_day - chrono::Duration::days(1), 20, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(110), dec!(110), dec!(110), dec!(110), dec!(1000)),
            ),
        ];
        rt.block_on(async {
            store
                .append_trade_bars(
                    &bars,
                    symbol.security_type(),
                    symbol.market().as_str(),
                    Resolution::Daily,
                    lean_core::TickType::Trade,
                )
                .await
        })
        .unwrap();

        let service = HistoryService::new(context(store));
        let latest = service
            .load_last_known_trade_bar(
                &symbol,
                Resolution::Daily,
                dt(second_day + chrono::Duration::days(1), 0, 0),
                DataNormalizationMode::Raw,
            )
            .unwrap();

        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0].close, dec!(110));
    }

    struct FixtureCustomSource {
        path: PathBuf,
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

    #[test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    fn custom_history_uses_subscription_fetch_and_reader() {
        let tmp = tempfile::tempdir().unwrap();
        let fixture_path = tmp.path().join("fixture.csv");
        std::fs::write(&fixture_path, "row\n").unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let Some(store) = rt.block_on(crate::test_support::connect_test_store()) else {
            return;
        };
        let symbol = Symbol::create_base("fixture", "ALT", &Market::usa());
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
        let subscription = SubscriptionDataConfig::new_custom(symbol, Resolution::Daily, metadata);
        let service = HistoryService::new(AlgorithmHistoryContext {
            data_root: PathBuf::new(),
            data_store: store,
            history_provider: None,
            custom_data_sources: vec![Arc::new(FixtureCustomSource { path: fixture_path })],
        });

        let points = service
            .load_custom_history_blocking(&subscription, day(2024, 1, 17), day(2024, 1, 17))
            .unwrap();

        assert_eq!(points.len(), 1);
        assert_eq!(points[0].value, dec!(7));
    }

    #[test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    fn history_count_returns_exact_requested_count_like_lean() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let Some(store) = rt.block_on(crate::test_support::connect_test_store()) else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let bars = (0..5)
            .map(|idx| {
                let date = day(2024, 1, 8 + idx);
                let price = dec!(100) + rust_decimal::Decimal::from(idx);
                TradeBar::new(
                    symbol.clone(),
                    dt(date, 20, 0),
                    TimeSpan::ONE_DAY,
                    TradeBarData::new(price, price, price, price, dec!(1000)),
                )
            })
            .collect::<Vec<_>>();
        rt.block_on(async {
            store
                .append_trade_bars(
                    &bars,
                    symbol.security_type(),
                    symbol.market().as_str(),
                    Resolution::Daily,
                    lean_core::TickType::Trade,
                )
                .await
        })
        .unwrap();
        let service = HistoryService::new(context(store));
        let mut algorithm = QcAlgorithm::new("test", dec!(100_000));
        algorithm.utc_time = dt(day(2024, 1, 15), 0, 0);
        algorithm.start_date = algorithm.utc_time;
        algorithm
            .subscription_manager
            .add(SubscriptionDataConfig::new_equity(
                symbol.clone(),
                Resolution::Daily,
                DataNormalizationMode::Raw,
            ));

        let history = service.history(&algorithm, &symbol, 3, Resolution::Daily);

        assert_eq!(history["close"].len(), 3);
        assert_eq!(history["close"], vec!["102", "103", "104"]);
    }

    #[test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    fn initialize_time_history_uses_start_date_when_frontier_is_epoch() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let Some(store) = rt.block_on(crate::test_support::connect_test_store()) else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let bar = TradeBar::new(
            symbol.clone(),
            dt(day(2024, 1, 12), 20, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        rt.block_on(async {
            store
                .append_trade_bars(
                    &[bar],
                    symbol.security_type(),
                    symbol.market().as_str(),
                    Resolution::Daily,
                    lean_core::TickType::Trade,
                )
                .await
        })
        .unwrap();
        let service = HistoryService::new(context(store));
        let mut algorithm = QcAlgorithm::new("test", dec!(100_000));
        algorithm.start_date = dt(day(2024, 1, 16), 0, 0);
        assert_eq!(algorithm.utc_time, DateTime::EPOCH);
        algorithm
            .subscription_manager
            .add(SubscriptionDataConfig::new_equity(
                symbol.clone(),
                Resolution::Daily,
                DataNormalizationMode::Raw,
            ));

        let history = service.history(&algorithm, &symbol, 1, Resolution::Daily);

        assert_eq!(history["close"], vec!["100"]);
    }

    #[test]
    fn custom_history_count_is_sorted_by_end_time_like_lean() {
        let first =
            CustomDataPoint::empty(day(2024, 9, 27), Some(dt(day(2024, 9, 27), 10, 0)), dec!(1));
        let second =
            CustomDataPoint::empty(day(2024, 9, 30), Some(dt(day(2024, 9, 30), 11, 0)), dec!(2));
        let third =
            CustomDataPoint::empty(day(2024, 10, 1), Some(dt(day(2024, 10, 1), 17, 0)), dec!(3));
        let columns = custom_points_to_columns(&[first, second, third]);

        assert_eq!(
            columns["end_time"],
            vec![
                "2024-09-27T10:00:00+00:00",
                "2024-09-30T11:00:00+00:00",
                "2024-10-01T17:00:00+00:00"
            ]
        );
        assert_eq!(columns["value"], vec!["1", "2", "3"]);
    }
}
