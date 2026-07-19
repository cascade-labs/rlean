use crate::data_feed::DataFeedContext;
use crate::subscription_reader::SubscriptionStream;
use rlean_core::{DateTime, Result as LeanResult};
use rlean_data::Slice;
use std::time::Duration;

const STREAM_PROGRESS_WARNING_INTERVAL: Duration = Duration::from_secs(15);

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
                    // Do not await a stream that is already ahead of the
                    // minimum watermark. In an all-empty range that producer
                    // is gated on the lagging streams; awaiting it first would
                    // prevent us from consuming those lagging watermarks and
                    // deadlock the shared prefetch frontier.
                    .filter(|stream| {
                        min_watermark
                            .map(|minimum| {
                                stream
                                    .watermark()
                                    .map(|watermark| watermark <= minimum)
                                    .unwrap_or(true)
                            })
                            .unwrap_or(true)
                    })
                {
                    match tokio::time::timeout(
                        STREAM_PROGRESS_WARNING_INTERVAL,
                        stream.advance_until_progress(),
                    )
                    .await
                    {
                        Ok(result) => result?,
                        Err(_) => {
                            tracing::warn!(
                                symbol = %stream.config().symbol.value,
                                subscription_id = stream.config().unique_id(),
                                pending = stream.pending_len(),
                                exhausted = stream.is_exhausted(),
                                producer_finished = stream.producer_is_finished(),
                                watermark = ?stream.watermark(),
                                consumer_frontier = ?self.context.consumer_frontier_date(),
                                prefetch_ceiling = ?self.context.prefetch_ceiling_date(),
                                "subscription stream made no progress while synchronizer had no candidate"
                            );
                            continue;
                        }
                    }
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
                match tokio::time::timeout(
                    STREAM_PROGRESS_WARNING_INTERVAL,
                    stream.advance_until_progress(),
                )
                .await
                {
                    Ok(result) => result?,
                    Err(_) => {
                        tracing::warn!(
                            candidate = %candidate,
                            symbol = %stream.config().symbol.value,
                            subscription_id = stream.config().unique_id(),
                            pending = stream.pending_len(),
                            exhausted = stream.is_exhausted(),
                            producer_finished = stream.producer_is_finished(),
                            watermark = ?stream.watermark(),
                            consumer_frontier = ?self.context.consumer_frontier_date(),
                            prefetch_ceiling = ?self.context.prefetch_ceiling_date(),
                            "subscription stream made no progress while synchronizer waited for candidate"
                        );
                    }
                }
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
