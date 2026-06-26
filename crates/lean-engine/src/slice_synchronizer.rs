use crate::subscription_reader::SubscriptionStream;
use lean_core::{DateTime, Result as LeanResult};
use lean_data::Slice;

/// Synchronizes multiple subscription streams into time-aligned `Slice`s,
/// mirroring C# LEAN's subscription synchronization.
pub struct SliceSynchronizer {
    streams: Vec<SubscriptionStream>,
    end: DateTime,
}

impl SliceSynchronizer {
    pub fn new(streams: Vec<SubscriptionStream>, end: DateTime) -> Self {
        SliceSynchronizer { streams, end }
    }

    pub async fn next_slice(&mut self) -> LeanResult<Option<Slice>> {
        let mut frontier: Option<DateTime> = None;

        for stream in &mut self.streams {
            stream.fill_pending().await?;
            if let Some(point) = stream.peek() {
                let time = point.frontier_time();
                frontier = Some(match frontier {
                    Some(current) => current.min(time),
                    None => time,
                });
            }
        }

        let frontier = match frontier {
            Some(time) if time <= self.end => time,
            _ => return Ok(None),
        };

        let mut slice = Slice::new(frontier);
        for stream in &mut self.streams {
            while let Some(point) = stream.peek() {
                if point.frontier_time() != frontier {
                    break;
                }
                let point = stream.pop_next().await?.expect("peek implied pending data");
                point.add_to_slice(&mut slice);
            }
        }

        Ok(Some(slice))
    }

    pub fn streams(&self) -> &[SubscriptionStream] {
        &self.streams
    }

    pub fn streams_mut(&mut self) -> &mut [SubscriptionStream] {
        &mut self.streams
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscription_reader::SubscriptionStream;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{Market, Resolution, Symbol, TimeSpan};
    use lean_data::{SubscriptionDataConfig, TradeBar, TradeBarData};
    use lean_storage::{ParquetWriter, PathResolver, WriterConfig};
    use rust_decimal_macros::dec;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    async fn write_minute_bars(
        root: &PathBuf,
        symbol: &Symbol,
        date: NaiveDate,
        bars: Vec<TradeBar>,
    ) {
        let resolver = PathResolver::new(root);
        let writer = ParquetWriter::new(WriterConfig::default());
        let path = resolver.market_data_partition(symbol, Resolution::Minute, lean_core::TickType::Trade, date);
        writer.write_trade_bars(&bars, &path).unwrap();
    }

    #[tokio::test]
    async fn synchronizer_emits_all_intraday_bars() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                dt(day, 9, 31),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(day, 9, 32),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(2), dec!(100)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(day, 9, 33),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(3), dec!(3), dec!(3), dec!(3), dec!(100)),
            ),
        ];
        write_minute_bars(&root, &symbol, day, bars).await;

        let reader = Arc::new(lean_storage::ParquetReader::new());
        let resolver = PathResolver::new(&root);
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            lean_core::DataNormalizationMode::Raw,
        );
        let start = dt(day, 0, 0);
        let end = dt(day, 23, 59);
        let stream = SubscriptionStream::new(config, reader, resolver, start, end);
        let mut sync = SliceSynchronizer::new(vec![stream], end);

        let mut closes = Vec::new();
        while let Some(slice) = sync.next_slice().await.unwrap() {
            if let Some(bar) = slice.get_bar(&symbol) {
                closes.push(bar.close);
            }
        }
        assert_eq!(closes, vec![dec!(1), dec!(2), dec!(3)]);
    }
}
