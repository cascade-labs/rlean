use crate::data_feed::DataFeedContext;
use crate::subscription_reader::SubscriptionStream;
use rlean_core::{DateTime, LeanError, Result as LeanResult};
use rlean_data::Slice;

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
                if !await_any_candidate(&mut self.streams, min_watermark).await? {
                    return Ok(None);
                }
                continue;
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
                    // No wall-clock bound and no periodic logging: waiting on
                    // an active producer is normal operation (see
                    // `await_any_candidate`). Transport failures resolve the
                    // await with the producer's error.
                    stream.advance_until_progress().await?;
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

/// Await the lagging streams until one has a queued point or every stream is
/// exhausted. Returns `false` only when every stream has resolved (exhausted)
/// and no candidate can ever arrive.
///
/// There is deliberately no wall-clock bound here: an ACTIVE in-flight
/// provider request is normal operation and is awaited silently until it
/// resolves — with rows, with an empty result, or with a transport error the
/// producer propagates through the channel. The 2026-07-27 lost-signal
/// incident was exactly a wall-clock bound racing a live request: the seed's
/// sidecar gap-fill was actively fetching (19-26.5s) when a 15s "no progress"
/// bailout concluded "no candidate" and the seed resolved empty, seconds
/// before the data arrived. Liveness is "the request is still in flight", not
/// "N seconds elapsed"; timeouts belong at the transport level only.
///
/// Split from `next_slice` so it can be driven under tokio's virtual clock
/// (`start_paused`) in tests without a sidecar session.
async fn await_any_candidate(
    streams: &mut [SubscriptionStream],
    min_watermark: Option<DateTime>,
) -> LeanResult<bool> {
    let mut advanced_any = false;
    for stream in streams
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
        // Resolves when the producer delivers a message, fails with a
        // transport error, or completes and closes the channel (which marks
        // the stream exhausted). A producer with a request in flight keeps
        // the channel open, so parking here is exactly "await the request".
        stream.advance_until_progress().await?;
        advanced_any = true;
        if stream.peek().is_some() {
            break;
        }
    }
    Ok(advanced_any)
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
    use crate::subscription_data::SubscriptionDataPoint;
    use crate::subscription_reader::SubscriptionStreamMessage;
    use rlean_core::{DataNormalizationMode, Market, Resolution, Symbol, TimeSpan};
    use rlean_data::SubscriptionDataConfig;
    use rlean_data_tables::{TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// A `tracing` writer that captures emitted events into a shared buffer so
    /// a test can assert what was (or was not) logged while waiting.
    #[derive(Clone)]
    struct BufferWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for BufferWriter {
        type Writer = BufferWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn test_stream(
        ticker: &str,
    ) -> (
        SubscriptionStream,
        mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
        Symbol,
    ) {
        let symbol = Symbol::create_equity(ticker, &Market::usa());
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let (sender, receiver) = mpsc::channel(16);
        let stream = SubscriptionStream::from_channel_for_tests(config, receiver);
        (stream, sender, symbol)
    }

    fn sample_point(symbol: Symbol) -> SubscriptionDataPoint {
        SubscriptionDataPoint::TradeBar(TradeBar::new(
            symbol,
            DateTime::from_secs(1_700_000_000),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(70), dec!(70), dec!(70), dec!(70), dec!(1)),
        ))
    }

    // Regression for the 2026-07-27 lost-signal incident (TRMB/DBX): the seed's
    // history request WAS dispatched and the provider WAS actively gap-filling
    // (26.5s / 19s observed), but the synchronizer abandoned the wait on a
    // wall-clock bound and concluded "no candidate" while the request was still
    // in flight. An ACTIVE in-flight request must be awaited until it resolves
    // — rows, empty, or transport failure — never timed out from above. And
    // waiting on an active provider call is normal operation: it must be
    // SILENT, not a periodic WARN.
    #[tokio::test(start_paused = true)]
    async fn in_flight_request_is_awaited_to_completion_not_abandoned() {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(BufferWriter(buffer.clone()))
            .finish();

        let (stream, sender, symbol) = test_stream("TRMB");
        let mut streams = vec![stream];
        let point = sample_point(symbol);
        let expected_frontier = point.frontier_time();
        tokio::spawn(async move {
            // Resolves well past the old 15s no-progress bound, matching the
            // observed 19-26.5s sidecar gap-fills that were abandoned live.
            tokio::time::sleep(Duration::from_secs(20)).await;
            let _ = sender
                .send(Ok(SubscriptionStreamMessage::point(point)))
                .await;
        });

        let has_candidate = {
            let _guard = tracing::subscriber::set_default(subscriber);
            await_any_candidate(&mut streams, None)
                .await
                .expect("await must not fail")
        };

        assert!(
            has_candidate,
            "an in-flight provider request must be awaited to completion, \
             not abandoned on a wall-clock bound"
        );
        let queued = streams[0]
            .peek()
            .expect("the fetched bar must be queued as the candidate");
        assert_eq!(queued.frontier_time(), expected_frontier);

        let captured = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        assert!(
            captured.is_empty(),
            "waiting on an active provider call is normal operation and must \
             be silent: {captured}"
        );
    }

    // A transport failure resolves the request: it must abort promptly with the
    // producer's error, not wait for anything.
    #[tokio::test(start_paused = true)]
    async fn transport_failure_aborts_promptly_with_the_producer_error() {
        let (stream, sender, _) = test_stream("TRMB");
        let mut streams = vec![stream];
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let _ = sender
                .send(Err(LeanError::DataError(
                    "sidecar transport failed".to_string(),
                )))
                .await;
        });

        let started = tokio::time::Instant::now();
        let error = await_any_candidate(&mut streams, None)
            .await
            .expect_err("a transport failure must surface as an error");

        assert!(
            error.to_string().contains("sidecar transport failed"),
            "the producer's transport error must propagate: {error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "a resolved failure must abort promptly, elapsed {:?}",
            started.elapsed()
        );
    }

    // A request that resolves EMPTY (producer completes without points) is a
    // resolution, not a stall: the synchronizer concludes "no candidate"
    // promptly, which the seed path then reports as a loud lost-seed error.
    #[tokio::test(start_paused = true)]
    async fn resolved_empty_stream_concludes_no_candidate_promptly() {
        let (stream, sender, _) = test_stream("DBX");
        let mut streams = vec![stream];
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            drop(sender);
        });

        let started = tokio::time::Instant::now();
        let resolved = await_any_candidate(&mut streams, None)
            .await
            .expect("resolution must not fail");
        assert!(resolved, "the empty resolution itself is progress");
        assert!(
            streams[0].is_exhausted(),
            "the stream resolved to exhausted"
        );
        assert!(streams[0].peek().is_none());

        let no_candidate = await_any_candidate(&mut streams, None)
            .await
            .expect("exhausted streams must resolve");
        assert!(
            !no_candidate,
            "with every stream resolved there is provably no candidate"
        );
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "an empty resolution must conclude promptly, elapsed {:?}",
            started.elapsed()
        );
    }

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
