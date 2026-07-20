use crate::data_feed::DataFeedContext;
use crate::subscription_reader::SubscriptionStream;
use rlean_core::{DateTime, LeanError, Result as LeanResult};
use rlean_data::Slice;
use std::time::Duration;

const STREAM_PROGRESS_WARNING_INTERVAL: Duration = Duration::from_secs(15);

/// Synchronizes multiple subscription streams into time-aligned `Slice`s,
/// mirroring C# LEAN's subscription synchronization.
pub struct SliceSynchronizer {
    streams: Vec<SubscriptionStream>,
    end: DateTime,
    context: DataFeedContext,
    /// C# LEAN's `SubscriptionFrontierTimeProvider` ratchets UTC time with
    /// `max(earlyBird, utcNow)`. Keep the same state here so a subscription
    /// which becomes ready late can be drained without rewinding algorithm
    /// time.
    frontier: Option<DateTime>,
    last_emitted_slice_time: Option<DateTime>,
}

impl SliceSynchronizer {
    pub fn new(streams: Vec<SubscriptionStream>, end: DateTime, context: DataFeedContext) -> Self {
        SliceSynchronizer {
            streams,
            end,
            context,
            frontier: None,
            last_emitted_slice_time: None,
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

            let Some(early_bird) = frontier else {
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
            // Match C# LEAN's SubscriptionFrontierTimeProvider exactly: the
            // next frontier is the earliest current emit time, clamped to the
            // previous frontier. A late subscription is consumed at the
            // current frontier; it can never turn algorithm time backwards.
            let candidate = ratchet_frontier(self.frontier, early_bird);
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
            let mut slice = Slice::new(candidate);
            for stream in &mut self.streams {
                // Match C# LEAN's inner SubscriptionSynchronizer loop: consume
                // Current while EmitTimeUtc <= frontier, then MoveNext until
                // Current is beyond the frontier or the stream is complete.
                // An async producer's currently buffered queue is not itself a
                // completion barrier, so keep pumping until a later point or an
                // inclusive watermark proves this frontier complete.
                loop {
                    stream.drain_available_messages()?;
                    while let Some(point) = stream.peek() {
                        if !is_due_at_frontier(point.frontier_time(), candidate) {
                            break;
                        }
                        let point = stream.pop_pending().expect("peek implied pending data");
                        point.add_to_slice(&mut slice);
                    }
                    if stream.is_synchronized_through(candidate) {
                        break;
                    }
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
                                "subscription stream made no progress while synchronizer drained through candidate"
                            );
                        }
                    }
                }
            }
            if let Some(previous) = self.last_emitted_slice_time {
                if candidate < previous {
                    return Err(LeanError::DataError(format!(
                        "subscription synchronizer attempted to emit a backward slice: previous={previous}, candidate={candidate}"
                    )));
                }
            }
            self.frontier = Some(candidate);
            self.last_emitted_slice_time = Some(candidate);
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

fn ratchet_frontier(current: Option<DateTime>, early_bird: DateTime) -> DateTime {
    current
        .map(|frontier| frontier.max(early_bird))
        .unwrap_or(early_bird)
}

fn is_due_at_frontier(point_frontier: DateTime, frontier: DateTime) -> bool {
    point_frontier <= frontier
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_frontier_advancement_is_unchanged() {
        let first = DateTime::from_secs(1_000);
        let second = DateTime::from_secs(2_000);

        assert_eq!(ratchet_frontier(None, first), first);
        assert_eq!(ratchet_frontier(Some(first), second), second);
    }

    #[test]
    fn late_older_point_cannot_rewind_the_frontier() {
        let august = DateTime::from_secs(1_000);
        let december = DateTime::from_secs(2_000);

        let candidate = ratchet_frontier(Some(december), august);

        assert_eq!(candidate, december);
        assert!(is_due_at_frontier(august, candidate));
    }

    #[test]
    fn drain_policy_consumes_all_points_through_the_retained_frontier() {
        let august = DateTime::from_secs(1_000);
        let december = DateTime::from_secs(2_000);
        let january = DateTime::from_secs(3_000);
        let candidate = ratchet_frontier(Some(december), august);
        let pending = [august, december, january];

        let drained: Vec<_> = pending
            .into_iter()
            .take_while(|point| is_due_at_frontier(*point, candidate))
            .collect();

        assert_eq!(candidate, december);
        assert_eq!(drained, vec![august, december]);
    }
}
