use crate::data_feed::DataFeedContext;
use crate::subscription_reader::SubscriptionStream;
use lean_core::{DateTime, Result as LeanResult};
use lean_data::Slice;

/// Synchronizes multiple subscription streams into time-aligned `Slice`s,
/// mirroring C# LEAN's subscription synchronization.
pub struct SliceSynchronizer {
    streams: Vec<SubscriptionStream>,
    end: DateTime,
    context: DataFeedContext,
}

impl SliceSynchronizer {
    pub fn new(streams: Vec<SubscriptionStream>, end: DateTime, context: DataFeedContext) -> Self {
        SliceSynchronizer {
            streams,
            end,
            context,
        }
    }

    pub async fn next_slice(&mut self) -> LeanResult<Option<Slice>> {
        loop {
            let mut frontier: Option<DateTime> = None;

            for stream in &mut self.streams {
                stream.drain_available_messages()?;
                if let Some(point) = stream.peek() {
                    let time = point.frontier_time();
                    frontier = Some(match frontier {
                        Some(current) => current.min(time),
                        None => time,
                    });
                }
            }

            let Some(candidate) = frontier else {
                // No stream has a current point. Ratchet the frontier to the
                // minimum watermark so producers gated on the prefetch horizon
                // wake up and load the next partitions; otherwise a data gap
                // longer than the horizon deadlocks producer<->consumer.
                let min_watermark = self
                    .streams
                    .iter()
                    .filter(|stream| !stream.is_exhausted())
                    .filter_map(|stream| stream.watermark())
                    .min();
                if let Some(watermark) = min_watermark {
                    self.context.observe_consumer_frontier(watermark.date_utc());
                }
                let mut advanced_any = false;
                for stream in self
                    .streams
                    .iter_mut()
                    .filter(|stream| !stream.is_exhausted())
                {
                    stream.advance_until_progress().await?;
                    advanced_any = true;
                    if stream.peek().is_some() {
                        break;
                    }
                }
                if advanced_any {
                    continue;
                }
                return Ok(None);
            };
            if candidate > self.end {
                return Ok(None);
            }
            // LEAN's frontier is the candidate being synchronized (the minimum
            // current emit time), not the last emitted slice. Publish it before
            // waiting on any stream so a producer whose next partition is needed
            // for this candidate is never gated behind the prefetch horizon —
            // gating on the previous slice date deadlocks across market-closed
            // gaps (weekend + holiday) longer than the horizon.
            self.context.observe_consumer_frontier(candidate.date_utc());
            if let Some(stream) = self
                .streams
                .iter_mut()
                .find(|stream| !stream.is_ready_for(candidate))
            {
                stream.advance_until_progress().await?;
                continue;
            }

            let mut slice = Slice::new(candidate);
            for stream in &mut self.streams {
                while let Some(point) = stream.peek() {
                    if point.frontier_time() != candidate {
                        break;
                    }
                    let point = stream.pop_pending().expect("peek implied pending data");
                    point.add_to_slice(&mut slice);
                }
            }
            return Ok(Some(slice));
        }
    }

    pub fn streams(&self) -> &[SubscriptionStream] {
        &self.streams
    }

    pub fn streams_mut(&mut self) -> &mut [SubscriptionStream] {
        &mut self.streams
    }

    pub fn add_stream(&mut self, stream: SubscriptionStream) {
        self.streams.push(stream);
    }

    pub fn remove_stream(&mut self, subscription_id: u64) {
        self.streams
            .retain(|stream| stream.config().unique_id() != subscription_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_feed::DataFeedContext;
    use crate::subscription_reader::SubscriptionStream;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{Market, Resolution, Symbol, TimeSpan};
    use lean_data::{SubscriptionDataConfig, TradeBar, TradeBarData};
    use lean_storage::IcebergStore;
    use rust_decimal_macros::dec;

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    async fn write_minute_bars(store: &IcebergStore, symbol: &Symbol, bars: Vec<TradeBar>) {
        store
            .append_trade_bars(
                &bars,
                symbol.security_type(),
                symbol.market().as_str(),
                Resolution::Minute,
                lean_core::TickType::Trade,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires a REST catalog: set RLEAN_TEST_CATALOG"]
    async fn synchronizer_emits_all_intraday_bars() {
        let Some(store) = crate::test_support::connect_test_store().await else {
            return;
        };
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let bars = vec![
            TradeBar::new(
                symbol.clone(),
                dt(day, 14, 31),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(day, 14, 32),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(2), dec!(100)),
            ),
            TradeBar::new(
                symbol.clone(),
                dt(day, 14, 33),
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(3), dec!(3), dec!(3), dec!(3), dec!(100)),
            ),
        ];
        write_minute_bars(&store, &symbol, bars).await;

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            lean_core::DataNormalizationMode::Raw,
        );
        let start = dt(day, 0, 0);
        let end = dt(day, 23, 59);
        let context = DataFeedContext::new(store);
        let stream = SubscriptionStream::new(config, context.clone(), start, end);
        let mut sync = SliceSynchronizer::new(vec![stream], end, context);

        let mut closes = Vec::new();
        while let Some(slice) = sync.next_slice().await.unwrap() {
            if let Some(bar) = slice.get_bar(&symbol) {
                closes.push(bar.close);
            }
        }
        assert_eq!(closes, vec![dec!(1), dec!(2), dec!(3)]);
    }
}
