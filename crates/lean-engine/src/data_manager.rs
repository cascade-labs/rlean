use crate::slice_synchronizer::SliceSynchronizer;
use crate::subscription_reader::SubscriptionStream;
use lean_core::{DateTime, Result as LeanResult};
use lean_data::{Slice, SubscriptionDataConfig};
use lean_storage::{ParquetReader, PathResolver};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::debug;

/// Drives backtest data through LEAN-style subscription streams.
pub struct DataManager {
    reader: Arc<ParquetReader>,
    resolver: PathResolver,
    synchronizer: Option<SliceSynchronizer>,
}

impl DataManager {
    pub fn new(data_root: PathBuf) -> Self {
        DataManager {
            reader: Arc::new(ParquetReader::new()),
            resolver: PathResolver::new(data_root),
            synchronizer: None,
        }
    }

    /// Initialize subscription streams for the backtest period.
    pub async fn initialize_feed(
        &mut self,
        configs: &[SubscriptionDataConfig],
        start: DateTime,
        end: DateTime,
    ) -> LeanResult<()> {
        let streams = configs
            .iter()
            .cloned()
            .map(|config| {
                SubscriptionStream::new(
                    config,
                    self.reader.clone(),
                    self.resolver.clone(),
                    start,
                    end,
                )
            })
            .collect();
        self.synchronizer = Some(SliceSynchronizer::new(streams, end));
        debug!(
            "Initialized subscription feed with {} stream(s)",
            self.synchronizer
                .as_ref()
                .map(|s| s.streams().len())
                .unwrap_or(0)
        );
        Ok(())
    }

    /// Advance to the next synchronized slice across all subscriptions.
    pub async fn next_slice(&mut self) -> LeanResult<Option<Slice>> {
        match self.synchronizer.as_mut() {
            Some(sync) => sync.next_slice().await,
            None => Ok(None),
        }
    }

    pub fn resolver(&self) -> &PathResolver {
        &self.resolver
    }

    pub fn reader(&self) -> &ParquetReader {
        &self.reader
    }
}

#[cfg(test)]
mod tests {
    use super::DataManager;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{
        DataNormalizationMode, DateTime, Market, Resolution, Symbol, TickType, TimeSpan,
    };
    use lean_data::{SubscriptionDataConfig, Tick, TradeBar, TradeBarData};
    use lean_storage::{FactorFileEntry, ParquetWriter, PathResolver, WriterConfig};
    use rust_decimal_macros::dec;

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    #[tokio::test]
    async fn feed_skips_missing_daily_partitions_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let writer = ParquetWriter::new(WriterConfig::default());
        let symbol = Symbol::create_equity("XLC", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2018, 6, 18).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2018, 6, 19).unwrap();
        let day3 = NaiveDate::from_ymd_opt(2018, 6, 20).unwrap();

        let empty_path =
            resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, day1);
        writer.write_trade_bars(&[], &empty_path).unwrap();

        let day2_path =
            resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, day2);
        let bar = TradeBar::new(
            symbol.clone(),
            dt(day2, 16, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(50), dec!(50), dec!(50), dec!(50), dec!(1000)),
        );
        writer.write_trade_bars(&[bar], &day2_path).unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Adjusted,
        );
        let mut manager = DataManager::new(tmp.path().to_path_buf());
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
    async fn feed_emits_cross_midnight_daily_bar_on_session_frontier() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let writer = ParquetWriter::new(WriterConfig::default());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let day0 = NaiveDate::from_ymd_opt(2024, 1, 14).unwrap();

        let path =
            resolver.market_data_partition(&symbol, Resolution::Daily, TickType::Trade, day2);
        let bar = TradeBar::new(
            symbol.clone(),
            dt(day1, 20, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        writer.write_trade_bars(&[bar], &path).unwrap();

        let factor_path = resolver.factor_file("usa", "SPY");
        writer
            .write_factor_file(
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
                &factor_path,
            )
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Adjusted,
        );
        let mut manager = DataManager::new(tmp.path().to_path_buf());
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
    async fn feed_emits_all_hourly_bars_in_partition() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let writer = ParquetWriter::new(WriterConfig::default());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let path = resolver.market_data_partition(&symbol, Resolution::Hour, TickType::Trade, day);
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                dt(day, 10, 0),
                TimeSpan::ONE_HOUR,
                TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(day, 11, 0),
                TimeSpan::ONE_HOUR,
                TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(2), dec!(100)),
            ),
        ];
        writer.write_trade_bars(&bars, &path).unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Hour,
            DataNormalizationMode::Raw,
        );
        let mut manager = DataManager::new(tmp.path().to_path_buf());
        manager
            .initialize_feed(&[config], dt(day, 0, 0), dt(day, 23, 59))
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
    async fn feed_emits_tick_partition_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let resolver = PathResolver::new(tmp.path());
        let writer = ParquetWriter::new(WriterConfig::default());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let path = resolver.market_data_partition(&symbol, Resolution::Tick, TickType::Trade, day);
        let ticks = vec![
            Tick::trade(symbol.clone(), dt(day, 9, 31), dec!(1), dec!(100)),
            Tick::trade(symbol.clone(), dt(day, 9, 32), dec!(2), dec!(100)),
        ];
        writer.write_ticks(&ticks, &path).unwrap();

        let config = SubscriptionDataConfig {
            symbol: symbol.clone(),
            resolution: Resolution::Tick,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: false,
            extended_market_hours: false,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
        };
        let mut manager = DataManager::new(tmp.path().to_path_buf());
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
}
