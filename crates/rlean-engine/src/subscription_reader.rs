use crate::data_feed::DataFeedContext;
use crate::normalization::{normalize_quote_bar, normalize_trade_bar};
use crate::subscription_data::SubscriptionDataPoint;
use futures::StreamExt;
use rlean_core::{
    DateTime, LeanError, MarketHoursDatabase, Resolution, Result as LeanResult, SecurityType,
    SymbolOptionsExt,
};
use rlean_data::SubscriptionDataConfig;
use rlean_data_sidecar::{
    decode_batch, decode_factor_file_batch, CanonicalDataBatch, DeliveryMode, SubscriptionSpec,
    WireDataType,
};
use rlean_data_tables::FactorFileEntry;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(crate) enum SubscriptionStreamMessage {
    Point(Box<SubscriptionDataPoint>),
    Watermark(DateTime),
}

#[cfg(test)]
impl SubscriptionStreamMessage {
    pub(crate) fn point(point: SubscriptionDataPoint) -> Self {
        Self::Point(Box::new(point))
    }
}

/// One engine subscription backed by one registered sidecar subscription.
///
/// The producer requests bounded backtest windows and the channel supplies the
/// same backpressure that the synchronizer used before the Flight migration.
/// There is no storage/provider fallback: a sidecar session is mandatory.
pub struct SubscriptionStream {
    config: SubscriptionDataConfig,
    receiver: mpsc::Receiver<LeanResult<SubscriptionStreamMessage>>,
    cancel: Option<oneshot::Sender<()>>,
    pending: VecDeque<SubscriptionDataPoint>,
    watermark: Option<DateTime>,
    exhausted: bool,
    producer_error: Option<LeanError>,
}

impl SubscriptionStream {
    pub fn new(
        config: SubscriptionDataConfig,
        context: DataFeedContext,
        start: DateTime,
        end: DateTime,
    ) -> Self {
        Self::new_inner(config, context, start, end, false)
    }

    /// Create a stream added while a backtest is already advancing.
    ///
    /// LEAN makes newly-selected securities available from the current
    /// frontier; it does not read weeks of future data merely to establish the
    /// current price. Keep this stream demand-driven until the frontier moves.
    pub fn new_dynamic(
        config: SubscriptionDataConfig,
        context: DataFeedContext,
        start: DateTime,
        end: DateTime,
    ) -> Self {
        Self::new_inner(config, context, start, end, true)
    }

    fn new_inner(
        config: SubscriptionDataConfig,
        context: DataFeedContext,
        start: DateTime,
        end: DateTime,
        _dynamic_subscription: bool,
    ) -> Self {
        let capacity = context.channel_capacity();
        let (sender, receiver) = mpsc::channel(capacity);
        let (cancel, cancelled) = oneshot::channel();
        let producer_config = config.clone();
        // The producer task is detached: its lifetime is governed by the
        // cancel channel (see `Drop`) and its completion closes `sender`,
        // which is what marks the stream exhausted.
        tokio::spawn(async move {
            if let Err(error) =
                produce(producer_config, context, start, end, &sender, cancelled).await
            {
                let _ = sender.send(Err(error)).await;
            }
        });
        Self {
            config,
            receiver,
            cancel: Some(cancel),
            pending: VecDeque::new(),
            watermark: None,
            exhausted: false,
            producer_error: None,
        }
    }

    /// Build a stream fed by a bare channel instead of a sidecar-backed
    /// producer, so synchronizer wait behavior can be driven under tokio's
    /// virtual clock in tests. The test owns the sender: an in-flight provider
    /// request is "sender alive, nothing sent yet", a transport failure is an
    /// `Err` message, and a resolved-empty request is dropping the sender.
    #[cfg(test)]
    pub(crate) fn from_channel_for_tests(
        config: SubscriptionDataConfig,
        receiver: mpsc::Receiver<LeanResult<SubscriptionStreamMessage>>,
    ) -> Self {
        Self {
            config,
            receiver,
            cancel: None,
            pending: VecDeque::new(),
            watermark: None,
            exhausted: false,
            producer_error: None,
        }
    }

    pub fn config(&self) -> &SubscriptionDataConfig {
        &self.config
    }

    pub fn peek(&self) -> Option<&SubscriptionDataPoint> {
        self.pending.front()
    }

    pub fn is_exhausted(&self) -> bool {
        self.exhausted && self.pending.is_empty()
    }

    pub fn watermark(&self) -> Option<DateTime> {
        self.watermark
    }

    /// Whether this stream has proved that no further point can be emitted at
    /// or before `frontier`.
    ///
    /// A queued point beyond the frontier proves ordering, an inclusive
    /// watermark proves the bounded range is complete, and exhaustion proves
    /// the entire stream is complete. Merely having a due point queued is not
    /// sufficient: after consuming it the synchronizer must continue pumping,
    /// just like C# LEAN repeatedly calls `MoveNext()` until Current is beyond
    /// the frontier.
    pub fn is_synchronized_through(&self, frontier: DateTime) -> bool {
        self.exhausted
            || self
                .peek()
                .map(|point| point.frontier_time() > frontier)
                .unwrap_or(false)
            || self
                .watermark
                .map(|watermark| watermark >= frontier)
                .unwrap_or(false)
    }

    pub async fn advance_until_progress(&mut self) -> LeanResult<()> {
        self.receive_if_needed().await
    }

    pub fn pop_pending(&mut self) -> Option<SubscriptionDataPoint> {
        self.pending.pop_front()
    }

    pub async fn fill_pending(&mut self) -> LeanResult<()> {
        self.receive_if_needed().await
    }

    pub async fn pop_next(&mut self) -> LeanResult<Option<SubscriptionDataPoint>> {
        self.fill_pending().await?;
        let next = self.pending.pop_front();
        if self.pending.is_empty() {
            self.drain_available_messages()?;
        }
        Ok(next)
    }

    async fn receive_if_needed(&mut self) -> LeanResult<()> {
        if let Some(error) = self.producer_error.take() {
            return Err(error);
        }
        self.drain_available_messages()?;
        if !self.pending.is_empty() || self.exhausted {
            return Ok(());
        }
        match self.receiver.recv().await {
            Some(Ok(message)) => self.handle_message(message)?,
            Some(Err(error)) => {
                self.exhausted = true;
                return Err(error);
            }
            None => self.exhausted = true,
        }
        self.drain_available_messages()
    }

    pub fn drain_available_messages(&mut self) -> LeanResult<()> {
        loop {
            match self.receiver.try_recv() {
                Ok(Ok(message)) => self.handle_message(message)?,
                Ok(Err(error)) => {
                    self.exhausted = true;
                    return Err(error);
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.exhausted = true;
                    break;
                }
            }
        }
        Ok(())
    }

    fn handle_message(&mut self, message: SubscriptionStreamMessage) -> LeanResult<()> {
        match message {
            SubscriptionStreamMessage::Point(point) => {
                let frontier = point.frontier_time();
                validate_point_against_watermark(&self.config, frontier, self.watermark)?;
                self.pending.push_back(*point);
            }
            SubscriptionStreamMessage::Watermark(watermark) => {
                if let Some(current) = self.watermark {
                    if watermark < current {
                        return Err(LeanError::DataError(format!(
                            "subscription watermark moved backward: subscription_id={}, symbol={}, previous={}, incoming={}",
                            self.config.unique_id(), self.config.symbol.value, current, watermark
                        )));
                    }
                }
                self.watermark = Some(watermark);
            }
        }
        Ok(())
    }
}

fn watermark_contract_error(
    config: &SubscriptionDataConfig,
    point_frontier: DateTime,
    watermark: DateTime,
) -> LeanError {
    LeanError::DataError(format!(
        "subscription delivered data behind its watermark: subscription_id={}, symbol={}, point_frontier={}, watermark={}",
        config.unique_id(), config.symbol.value, point_frontier, watermark
    ))
}

fn validate_point_against_watermark(
    config: &SubscriptionDataConfig,
    point_frontier: DateTime,
    watermark: Option<DateTime>,
) -> LeanResult<()> {
    // `produce_registered` publishes a watermark only after its inclusive
    // bounded query is fully decoded and every point from that window has
    // already been sent on the same ordered channel. A subsequently received
    // point at or below that watermark therefore violates the stream contract.
    if let Some(watermark) = watermark {
        if point_frontier <= watermark {
            return Err(watermark_contract_error(config, point_frontier, watermark));
        }
    }
    Ok(())
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        // Let the producer cancel its in-flight sidecar query and explicitly
        // unregister the remote subscription. Aborting the task here skips the
        // cleanup after `produce_registered`, leaking both on the sidecar.
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

async fn produce(
    config: SubscriptionDataConfig,
    context: DataFeedContext,
    start: DateTime,
    end: DateTime,
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
    mut cancelled: oneshot::Receiver<()>,
) -> LeanResult<()> {
    let sidecar = &context.sidecar;
    let registration = sidecar
        .add_subscription(&config, DeliveryMode::Backtest)
        .await
        .map_err(data_error)?;
    let subscription_id = registration.subscription_id;
    let effective_start = effective_subscription_start(start, registration.available_from);
    tracing::debug!(
        subscription_id,
        symbol = %config.symbol.value,
        source = ?config.custom.as_ref().map(|custom| custom.source_type.as_str()),
        requested_start = %start,
        available_from = ?registration.available_from.map(|value| value.to_string()),
        effective_start = %effective_start,
        "registered backtest subscription"
    );
    let factor_rows_result = tokio::select! {
        _ = &mut cancelled => {
            let _ = sidecar.remove_subscription(subscription_id).await;
            return Ok(());
        }
        result = load_auxiliary_rows(&config, &context) => result,
    };
    let factor_rows = match factor_rows_result {
        Ok(rows) => rows,
        Err(error) => {
            let _ = sidecar.remove_subscription(subscription_id).await;
            return Err(error);
        }
    };
    let result = tokio::select! {
        _ = &mut cancelled => Ok(()),
        result = produce_registered(
            &config,
            &context,
            subscription_id,
            (effective_start, end),
            &factor_rows,
            sender,
        ) => result,
    };
    if let Err(error) = sidecar.remove_subscription(subscription_id).await {
        tracing::warn!(subscription_id, %error, "failed to remove sidecar subscription");
    }
    result
}

fn effective_subscription_start(start: DateTime, available_from: Option<DateTime>) -> DateTime {
    available_from
        .map(|available_from| start.max(available_from))
        .unwrap_or(start)
}

async fn load_auxiliary_rows(
    config: &SubscriptionDataConfig,
    context: &DataFeedContext,
) -> LeanResult<Vec<FactorFileEntry>> {
    if config.symbol.security_type() != SecurityType::Equity {
        return Ok(Vec::new());
    }
    let cache_key = auxiliary_cache_key(config);
    if let Some(rows) = context.cached_auxiliary_factor_rows(&cache_key) {
        return Ok(rows);
    }

    let mut factor_spec = SubscriptionSpec::from(config);
    factor_spec.config_id ^= 0xfac7_0000_0000_0000;
    factor_spec.data_type = WireDataType::FactorFile as i32;
    factor_spec.resolution = 4;
    factor_spec.tick_type = 0;
    let factor_batches = query_auxiliary(context, factor_spec).await?;
    let mut factor_rows = Vec::new();
    for batch in &factor_batches {
        factor_rows.extend(decode_factor_file_batch(batch).map_err(data_error)?);
    }
    factor_rows.sort_by_key(|row| row.date);

    tracing::info!(
        ticker = %config.symbol.permtick,
        factor_rows = factor_rows.len(),
        "loaded equity factor rows from sidecar"
    );
    if config.normalization_mode != rlean_core::DataNormalizationMode::Raw && factor_rows.is_empty()
    {
        context.record_unadjusted_equity(config.symbol.permtick.as_ref());
    }
    context.cache_auxiliary_factor_rows(cache_key, factor_rows.clone());
    Ok(factor_rows)
}

fn auxiliary_cache_key(config: &SubscriptionDataConfig) -> String {
    format!(
        "{}:{}:{:?}",
        config.symbol.id.sid, config.symbol.permtick, config.normalization_mode
    )
}

async fn query_auxiliary(
    context: &DataFeedContext,
    spec: SubscriptionSpec,
) -> LeanResult<Vec<arrow::record_batch::RecordBatch>> {
    let sidecar = &context.sidecar;
    let registration = sidecar
        .add_subscription_spec(spec, DeliveryMode::Backtest)
        .await
        .map_err(data_error)?;
    let subscription_id = registration.subscription_id;
    let result = async {
        let mut stream = sidecar
            .query(subscription_id, 0, 0)
            .await
            .map_err(data_error)?;
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            batches.push(batch.map_err(data_error)?);
        }
        Ok(batches)
    }
    .await;
    if let Err(error) = sidecar.remove_subscription(subscription_id).await {
        tracing::warn!(subscription_id, %error, "failed to remove auxiliary sidecar subscription");
    }
    result
}

async fn produce_registered(
    config: &SubscriptionDataConfig,
    context: &DataFeedContext,
    subscription_id: u64,
    range: (DateTime, DateTime),
    factor_rows: &[FactorFileEntry],
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<()> {
    let (start, end) = range;
    let wire_type = WireDataType::try_from(SubscriptionSpec::from(config).data_type)
        .map_err(|value| LeanError::DataError(format!("unknown sidecar data type {value}")))?;
    let mut window_start = start.date_utc();
    let end_date = effective_subscription_end(config, end.date_utc());
    let mut last_point: Option<SubscriptionDataPoint> = None;
    let mut last_real_frontier: Option<DateTime> = None;

    while window_start <= end_date {
        let window_end = backtest_window_candidate(config.resolution, window_start).min(end_date);
        let mut batches = context
            .sidecar
            .query(
                subscription_id,
                partition_day_start(window_start).0,
                partition_day_end(window_end).0,
            )
            .await
            .map_err(data_error)?;
        let mut points = Vec::new();
        while let Some(batch) = batches.next().await {
            let batch = batch.map_err(data_error)?;
            points.extend(decode_points(
                config,
                wire_type,
                batch,
                factor_rows,
                &context.market_hours_database,
            )?);
        }
        // Flight is free to split one daily cross-section across record
        // batches. Reassemble all pieces before the synchronizer sees it so a
        // universe selector always receives the complete snapshot.
        points = coalesce_fundamental_snapshots(points);
        points.sort_by_key(SubscriptionDataPoint::frontier_time);
        points = deduplicate_points(config, points);
        tracing::debug!(
            subscription_id,
            symbol = %config.symbol.value,
            source = ?config.custom.as_ref().map(|custom| custom.source_type.as_str()),
            window_start = %window_start,
            window_end = %window_end,
            points = points.len(),
            first_frontier = ?points.first().map(SubscriptionDataPoint::frontier_time),
            last_frontier = ?points.last().map(SubscriptionDataPoint::frontier_time),
            "decoded backtest subscription window"
        );
        for point in points {
            let frontier = point.frontier_time();
            if frontier < start || frontier > end {
                continue;
            }
            let is_out_of_order_or_duplicate = last_real_frontier
                .map(|last| {
                    if config.data_kind == rlean_data::SubscriptionDataKind::Custom {
                        frontier < last
                    } else {
                        frontier <= last
                    }
                })
                .unwrap_or(false);
            if is_out_of_order_or_duplicate && !config.resolution.is_tick() {
                continue;
            }
            if let Some(previous) = last_point.as_ref() {
                send_fill_forward_before(
                    config,
                    &context.market_hours_database,
                    previous,
                    frontier,
                    end,
                    sender,
                )
                .await?;
            }
            if sender
                .send(Ok(SubscriptionStreamMessage::Point(Box::new(
                    point.clone(),
                ))))
                .await
                .is_err()
            {
                return Ok(());
            }
            last_real_frontier = Some(frontier);
            last_point = Some(point);
        }

        let watermark = partition_day_end(window_end);
        // C# LEAN places FillForwardEnumerator in front of the subscription
        // synchronizer, so every synthetic point covered by a frontier is
        // emitted before the synchronizer can advance beyond that frontier.
        // Our source is queried in bounded windows. Complete fill-forward for
        // the proven window before publishing its inclusive watermark so the
        // next window can never manufacture data behind that watermark.
        if let Some(previous) = last_point.as_ref() {
            if let Some(fill) = send_fill_forward_through(
                config,
                &context.market_hours_database,
                previous,
                watermark.min(end),
                end,
                sender,
            )
            .await?
            {
                last_point = Some(fill);
            }
        }
        if sender
            .send(Ok(SubscriptionStreamMessage::Watermark(watermark)))
            .await
            .is_err()
        {
            return Ok(());
        }
        window_start = match window_end.succ_opt() {
            Some(next) => next,
            None => break,
        };
    }
    Ok(())
}

fn decode_points(
    config: &SubscriptionDataConfig,
    wire_type: WireDataType,
    batch: arrow::record_batch::RecordBatch,
    factor_rows: &[FactorFileEntry],
    market_hours_database: &MarketHoursDatabase,
) -> LeanResult<Vec<SubscriptionDataPoint>> {
    let decoded = decode_batch(wire_type, batch, &config.symbol).map_err(data_error)?;
    match decoded {
        CanonicalDataBatch::TradeBars(rows) => Ok(rows
            .into_iter()
            .filter_map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                normalize_trade_bar(&mut bar, config.normalization_mode, factor_rows);
                bar.is_valid()
                    .then_some(SubscriptionDataPoint::TradeBar(bar))
            })
            .collect()),
        CanonicalDataBatch::QuoteBars(rows) => Ok(rows
            .into_iter()
            .map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                normalize_quote_bar(&mut bar, config.normalization_mode, factor_rows);
                SubscriptionDataPoint::QuoteBar(bar)
            })
            .collect()),
        CanonicalDataBatch::Ticks(rows) => Ok(rows
            .into_iter()
            .map(|mut tick| {
                tick.venue.get_or_insert_with(|| config.venue.clone());
                SubscriptionDataPoint::Tick(tick)
            })
            .collect()),
        CanonicalDataBatch::Custom(rows) | CanonicalDataBatch::Universe(rows) => {
            let ticker = config
                .custom
                .as_ref()
                .map(|custom| custom.ticker.clone())
                .unwrap_or_else(|| config.symbol.value.to_string());
            Ok(rows
                .into_iter()
                .map(|mut point| {
                    point.venue.get_or_insert_with(|| config.venue.clone());
                    SubscriptionDataPoint::CustomData {
                        symbol: config.symbol.clone(),
                        ticker: ticker.clone(),
                        point,
                    }
                })
                .collect())
        }
        CanonicalDataBatch::Fundamentals(rows) => {
            let mut snapshots: BTreeMap<DateTime, Vec<rlean_data::FundamentalData>> =
                BTreeMap::new();
            for row in rows {
                snapshots.entry(row.end_time).or_default().push(row);
            }
            Ok(snapshots
                .into_iter()
                .map(
                    |(frontier_time, data)| SubscriptionDataPoint::FundamentalUniverse {
                        data,
                        frontier_time,
                    },
                )
                .collect())
        }
        CanonicalDataBatch::OptionUniverse(rows) => {
            let chains = crate::option_universe::option_chains_from_rows(config, rows)
                .map_err(data_error)?;
            chains
                .into_iter()
                .map(|(date, chain)| {
                    let frontier_date = date.succ_opt().unwrap_or(date);
                    // LEAN assigns BaseChainUniverseData the exchange data
                    // time zone, so EndTime at the following midnight is
                    // converted from exchange-local time to UTC. A raw UTC
                    // midnight appears on the previous algorithm date for US
                    // markets and breaks same-day expiry filters.
                    let frontier_time = market_hours_database
                        .exchange_hours(&config.symbol)
                        .local_midnight_utc(frontier_date)
                        .ok_or_else(|| {
                            LeanError::DataError(format!(
                                "invalid option-universe exchange timezone for {}",
                                config.symbol.value
                            ))
                        })?;
                    Ok(SubscriptionDataPoint::OptionChain {
                        canonical_permtick: config.symbol.permtick.to_string(),
                        chain: Arc::new(chain),
                        frontier_time,
                    })
                })
                .collect()
        }
        CanonicalDataBatch::RiskFreeInterestRates(_) | CanonicalDataBatch::RecordBatch(_) => {
            Err(LeanError::DataError(format!(
                "sidecar data type {wire_type:?} is not consumable by a subscription"
            )))
        }
    }
}

fn coalesce_fundamental_snapshots(
    points: Vec<SubscriptionDataPoint>,
) -> Vec<SubscriptionDataPoint> {
    let mut snapshots: BTreeMap<DateTime, Vec<rlean_data::FundamentalData>> = BTreeMap::new();
    let mut other = Vec::with_capacity(points.len());
    for point in points {
        match point {
            SubscriptionDataPoint::FundamentalUniverse {
                data,
                frontier_time,
            } => snapshots.entry(frontier_time).or_default().extend(data),
            point => other.push(point),
        }
    }
    other.extend(snapshots.into_iter().map(|(frontier_time, data)| {
        SubscriptionDataPoint::FundamentalUniverse {
            data,
            frontier_time,
        }
    }));
    other
}

fn deduplicate_points(
    config: &SubscriptionDataConfig,
    points: Vec<SubscriptionDataPoint>,
) -> Vec<SubscriptionDataPoint> {
    // Multiple custom records at the same EndTime are independent events, not
    // duplicate bars. They must arrive together in Slice.custom_data.
    if config.resolution.is_tick() || config.data_kind == rlean_data::SubscriptionDataKind::Custom {
        return points;
    }
    let mut by_frontier = BTreeMap::new();
    for point in points {
        by_frontier.insert(point.frontier_time(), point);
    }
    by_frontier.into_values().collect()
}

async fn send_fill_forward_before(
    config: &SubscriptionDataConfig,
    market_hours_database: &MarketHoursDatabase,
    previous: &SubscriptionDataPoint,
    next_real_frontier: DateTime,
    end: DateTime,
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<Option<SubscriptionDataPoint>> {
    send_fill_forward_until(
        config,
        market_hours_database,
        previous,
        next_real_frontier,
        false,
        end,
        sender,
    )
    .await
}

async fn send_fill_forward_through(
    config: &SubscriptionDataConfig,
    market_hours_database: &MarketHoursDatabase,
    previous: &SubscriptionDataPoint,
    frontier: DateTime,
    end: DateTime,
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<Option<SubscriptionDataPoint>> {
    send_fill_forward_until(
        config,
        market_hours_database,
        previous,
        frontier,
        true,
        end,
        sender,
    )
    .await
}

async fn send_fill_forward_until(
    config: &SubscriptionDataConfig,
    market_hours_database: &MarketHoursDatabase,
    previous: &SubscriptionDataPoint,
    limit: DateTime,
    include_limit: bool,
    end: DateTime,
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<Option<SubscriptionDataPoint>> {
    if config.resolution.is_tick() || !config.fill_data_forward {
        return Ok(None);
    }
    let Some(period) = config.resolution.to_time_span() else {
        return Ok(None);
    };
    let mut frontier = previous.frontier_time() + period;
    let mut last_fill = None;
    while frontier <= end && (frontier < limit || include_limit && frontier == limit) {
        if is_market_open(config, market_hours_database, frontier, period) {
            if let Some(fill) = fill_forward_point(previous, frontier, period) {
                if sender
                    .send(Ok(SubscriptionStreamMessage::Point(Box::new(fill.clone()))))
                    .await
                    .is_err()
                {
                    return Ok(last_fill);
                }
                last_fill = Some(fill);
            }
        }
        frontier = frontier + period;
    }
    Ok(last_fill)
}

fn is_market_open(
    config: &SubscriptionDataConfig,
    market_hours_database: &MarketHoursDatabase,
    frontier: DateTime,
    period: rlean_core::TimeSpan,
) -> bool {
    if config.symbol.security_type() != SecurityType::Equity {
        return true;
    }
    market_hours_database
        .exchange_hours(&config.symbol)
        .is_open_at(frontier - period)
}

fn backtest_window_candidate(
    resolution: Resolution,
    start: chrono::NaiveDate,
) -> chrono::NaiveDate {
    match resolution {
        Resolution::Daily => add_years_saturating(start, 1),
        Resolution::Hour => add_months_saturating(start, 1),
        // A 21-calendar-day minute window is normally about 15 US equity
        // sessions (5,850 rows), keeping each subscription comfortably below
        // the default 100,000-row prefetch budget while avoiding one provider
        // request and sidecar round trip per trading day.
        Resolution::Minute => add_days_saturating(start, 20),
        Resolution::Tick | Resolution::Second => start,
    }
}

fn effective_subscription_end(
    config: &SubscriptionDataConfig,
    requested_end: chrono::NaiveDate,
) -> chrono::NaiveDate {
    // Canonical option-universe symbols carry an option-shaped SID but do not
    // represent one expiring contract. Their stream spans the algorithm range;
    // only concrete contract subscriptions are capped at the contract expiry.
    if config.option_chain.is_some() {
        return requested_end;
    }
    config
        .symbol
        .option_symbol_id()
        .map(|contract| requested_end.min(contract.expiry))
        .unwrap_or(requested_end)
}

fn partition_day_start(date: chrono::NaiveDate) -> DateTime {
    DateTime::from(date.and_hms_opt(0, 0, 0).expect("valid day start"))
}

fn partition_day_end(date: chrono::NaiveDate) -> DateTime {
    DateTime::from(date.and_hms_opt(23, 59, 59).expect("valid day end"))
}

fn add_months_saturating(date: chrono::NaiveDate, months: u32) -> chrono::NaiveDate {
    date.checked_add_months(chrono::Months::new(months))
        .unwrap_or(chrono::NaiveDate::MAX)
}

fn add_days_saturating(date: chrono::NaiveDate, days: i64) -> chrono::NaiveDate {
    date.checked_add_signed(chrono::Duration::days(days))
        .unwrap_or(chrono::NaiveDate::MAX)
}

fn add_years_saturating(date: chrono::NaiveDate, years: i32) -> chrono::NaiveDate {
    date.with_year(date.year().saturating_add(years))
        .unwrap_or_else(|| {
            date.with_day(28)
                .and_then(|day| day.with_year(date.year().saturating_add(years)))
                .unwrap_or(chrono::NaiveDate::MAX)
        })
}

fn fill_forward_point(
    previous: &SubscriptionDataPoint,
    frontier: DateTime,
    period: rlean_core::TimeSpan,
) -> Option<SubscriptionDataPoint> {
    match previous {
        SubscriptionDataPoint::TradeBar(bar) => {
            let mut fill = bar.clone();
            fill.end_time = frontier;
            fill.time = frontier - period;
            fill.open = fill.close;
            fill.high = fill.close;
            fill.low = fill.close;
            fill.volume = rust_decimal::Decimal::ZERO;
            fill.period = period;
            Some(SubscriptionDataPoint::TradeBar(fill))
        }
        SubscriptionDataPoint::QuoteBar(bar) => {
            let mut fill = bar.clone();
            fill.end_time = frontier;
            fill.time = frontier - period;
            fill.period = period;
            Some(SubscriptionDataPoint::QuoteBar(fill))
        }
        SubscriptionDataPoint::Tick(_)
        | SubscriptionDataPoint::CustomData { .. }
        | SubscriptionDataPoint::FundamentalUniverse { .. }
        | SubscriptionDataPoint::OptionChain { .. } => None,
    }
}

fn data_error(error: impl std::fmt::Display) -> LeanError {
    LeanError::DataError(error.to_string())
}

use chrono::Datelike;

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{Market, OptionRight, OptionStyle, Symbol, TimeSpan};
    use rlean_data::{
        CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata, OptionChainFilterMetadata,
        OptionChainSubscriptionMetadata,
    };
    use rlean_data_tables::{CustomDataPoint, TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    #[test]
    fn non_tick_batches_are_deduplicated_by_frontier() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let time = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(9, 30, 0)
                .unwrap(),
        );
        let bar = TradeBar::new(
            symbol.clone(),
            time,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(1)),
        );
        let config = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Minute,
            rlean_core::DataNormalizationMode::Raw,
        );
        let rows = deduplicate_points(
            &config,
            vec![
                SubscriptionDataPoint::TradeBar(bar.clone()),
                SubscriptionDataPoint::TradeBar(bar),
            ],
        );
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn custom_events_at_the_same_frontier_are_preserved() {
        let symbol = Symbol::create_base("unusual_whales", "flow_alerts", &Market::usa());
        let time = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2026, 4, 1)
                .unwrap()
                .and_hms_opt(9, 31, 0)
                .unwrap(),
        );
        let metadata = CustomSubscriptionMetadata {
            source_type: "unusual_whales".to_string(),
            ticker: "flow_alerts".to_string(),
            config: CustomDataConfig {
                ticker: "flow_alerts".to_string(),
                source_type: "unusual_whales".to_string(),
                resolution: Resolution::Minute,
                properties: HashMap::new(),
                query: CustomDataQuery::default(),
            },
            dynamic_query: CustomDataQuery::default(),
        };
        let config =
            SubscriptionDataConfig::new_custom(symbol.clone(), Resolution::Minute, metadata);
        let point =
            CustomDataPoint::with_lean_defaulting(Some(time), Some(time), dec!(1), HashMap::new())
                .unwrap();
        let event = || SubscriptionDataPoint::CustomData {
            symbol: symbol.clone(),
            ticker: "flow_alerts".to_string(),
            point: point.clone(),
        };

        let rows = deduplicate_points(&config, vec![event(), event()]);

        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn minute_backtest_queries_use_bounded_multi_day_windows() {
        let start = chrono::NaiveDate::from_ymd_opt(2022, 1, 3).unwrap();

        assert_eq!(
            backtest_window_candidate(Resolution::Minute, start),
            chrono::NaiveDate::from_ymd_opt(2022, 1, 23).unwrap()
        );
        assert_eq!(backtest_window_candidate(Resolution::Second, start), start);
        assert_eq!(backtest_window_candidate(Resolution::Tick, start), start);
    }

    #[test]
    fn canonical_option_universe_is_not_capped_by_placeholder_sid_expiry() {
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_option(
            underlying,
            &Market::usa(),
            chrono::NaiveDate::MIN,
            dec!(0),
            OptionRight::Call,
            OptionStyle::American,
        );
        let config = SubscriptionDataConfig::new_option_chain(
            canonical,
            Resolution::Minute,
            OptionChainSubscriptionMetadata {
                canonical_permtick: "?SPY".to_string(),
                underlying_ticker: "SPY".to_string(),
                filter: OptionChainFilterMetadata {
                    min_strike_rank: -5,
                    max_strike_rank: 5,
                    min_expiry_days: 0,
                    max_expiry_days: 0,
                },
            },
        );
        let requested_end = chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap();

        assert_eq!(
            effective_subscription_end(&config, requested_end),
            requested_end
        );
    }

    #[test]
    fn daily_and_hour_queries_use_stable_windows_not_consumer_frontier_fragments() {
        let start = chrono::NaiveDate::from_ymd_opt(2019, 4, 9).unwrap();

        assert_eq!(
            backtest_window_candidate(Resolution::Daily, start),
            chrono::NaiveDate::from_ymd_opt(2020, 4, 9).unwrap()
        );
        assert_eq!(
            backtest_window_candidate(Resolution::Hour, start),
            chrono::NaiveDate::from_ymd_opt(2019, 5, 9).unwrap()
        );
    }

    #[test]
    fn option_contract_queries_stop_at_expiration() {
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let expiry = chrono::NaiveDate::from_ymd_opt(2024, 7, 24).unwrap();
        let contract = Symbol::create_option(
            underlying,
            &Market::usa(),
            expiry,
            dec!(531),
            OptionRight::Call,
            OptionStyle::American,
        );
        let config = SubscriptionDataConfig::new_option(contract, Resolution::Minute);

        assert_eq!(
            effective_subscription_end(
                &config,
                chrono::NaiveDate::from_ymd_opt(2026, 7, 14).unwrap()
            ),
            expiry
        );
    }

    #[test]
    fn provider_availability_clamps_subscription_start() {
        let requested = partition_day_start(chrono::NaiveDate::from_ymd_opt(2000, 1, 1).unwrap());
        let available = partition_day_start(chrono::NaiveDate::from_ymd_opt(2023, 8, 16).unwrap());

        assert_eq!(
            effective_subscription_start(requested, Some(available)),
            available
        );
        assert_eq!(effective_subscription_start(available, None), available);
    }

    #[test]
    fn watermark_is_an_inclusive_stream_barrier() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let config = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Minute,
            rlean_core::DataNormalizationMode::Raw,
        );
        let watermark = DateTime::from_secs(2_000);

        let behind =
            validate_point_against_watermark(&config, DateTime::from_secs(1_000), Some(watermark))
                .unwrap_err();
        let equal =
            validate_point_against_watermark(&config, watermark, Some(watermark)).unwrap_err();

        assert!(behind
            .to_string()
            .contains("subscription delivered data behind its watermark"));
        assert!(equal
            .to_string()
            .contains("subscription delivered data behind its watermark"));
        assert!(validate_point_against_watermark(
            &config,
            DateTime::from_secs(3_000),
            Some(watermark)
        )
        .is_ok());
    }

    #[tokio::test]
    async fn fill_forward_completes_window_before_inclusive_watermark() {
        let symbol = Symbol::create_equity("ROIV", &Market::usa());
        let mut config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            rlean_core::DataNormalizationMode::Raw,
        );
        config.fill_data_forward = true;
        let period = TimeSpan::ONE_MINUTE;
        let last_real_end = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 7, 19)
                .unwrap()
                .and_hms_opt(19, 5, 0)
                .unwrap(),
        );
        let previous = SubscriptionDataPoint::TradeBar(TradeBar::new(
            symbol,
            last_real_end - period,
            period,
            TradeBarData::new(dec!(11), dec!(11), dec!(11), dec!(11), dec!(100)),
        ));
        let watermark = partition_day_end(chrono::NaiveDate::from_ymd_opt(2024, 7, 19).unwrap());
        let (sender, mut receiver) = mpsc::channel(128);

        let last_fill = send_fill_forward_through(
            &config,
            MarketHoursDatabase::global().as_ref(),
            &previous,
            watermark,
            watermark,
            &sender,
        )
        .await
        .unwrap()
        .expect("the incomplete regular session should be filled");
        sender
            .send(Ok(SubscriptionStreamMessage::Watermark(watermark)))
            .await
            .unwrap();

        let expected_close = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 7, 19)
                .unwrap()
                .and_hms_opt(20, 0, 0)
                .unwrap(),
        );
        assert_eq!(last_fill.frontier_time(), expected_close);

        let mut point_count = 0;
        let mut saw_watermark = false;
        while let Ok(message) = receiver.try_recv() {
            match message.unwrap() {
                SubscriptionStreamMessage::Point(point) => {
                    assert!(!saw_watermark, "fill-forward arrived after its watermark");
                    assert!(point.frontier_time() <= watermark);
                    point_count += 1;
                }
                SubscriptionStreamMessage::Watermark(incoming) => {
                    assert_eq!(incoming, watermark);
                    saw_watermark = true;
                }
            }
        }
        assert_eq!(point_count, 55);
        assert!(saw_watermark);

        let next_real = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 7, 22)
                .unwrap()
                .and_hms_opt(13, 31, 0)
                .unwrap(),
        );
        assert!(send_fill_forward_before(
            &config,
            MarketHoursDatabase::global().as_ref(),
            &last_fill,
            next_real,
            next_real,
            &sender,
        )
        .await
        .unwrap()
        .is_none());
    }
}
