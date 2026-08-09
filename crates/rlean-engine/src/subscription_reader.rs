use crate::data_feed::DataFeedContext;
use crate::normalization::{normalize_quote_bar, normalize_trade_bar};
use crate::subscription_data::SubscriptionDataPoint;
use rlean_core::{
    DateTime, LeanError, MarketHoursDatabase, Resolution, Result as LeanResult, SymbolOptionsExt,
    TickType, TimeSpan,
};
use rlean_data::SubscriptionDataConfig;
use rlean_data_providers::{HistoricalData, HistoricalDataProvider, HistoryRequest};
use rlean_data_tables::FactorFileEntry;
use std::collections::{BTreeMap, HashSet, VecDeque};
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

/// One engine subscription backed by the selected historical provider.
///
/// The producer requests bounded backtest windows and the channel supplies the
/// same backpressure as C# LEAN's pull enumerators.
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

    /// Build a stream fed by a bare channel instead of a provider-backed
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
        // Let the producer cancel its in-flight provider query.
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
    let provider = context.historical_provider.clone();
    let request = HistoryRequest::new(config.clone(), start, end).map_err(data_error)?;
    if !provider.supports(&request) {
        return Err(LeanError::DataError(format!(
            "historical provider '{}' does not support {:?} {:?} data for {}",
            provider.name(),
            config.symbol.security_type(),
            config.tick_type,
            config.symbol.value
        )));
    }
    let auxiliary_key = format!("{}:{}", config.symbol.market(), config.symbol.permtick);
    let factor_rows = if config.symbol.security_type() == rlean_core::SecurityType::Equity {
        if let Some(rows) = context.cached_auxiliary_factor_rows(&auxiliary_key) {
            rows
        } else {
            let rows = provider
                .get_factor_file(&config.symbol)
                .await
                .map_err(data_error)?
                .unwrap_or_default();
            context.cache_auxiliary_factor_rows(auxiliary_key.clone(), rows.clone());
            rows
        }
    } else {
        Vec::new()
    };
    let map_rows = if config.symbol.security_type() == rlean_core::SecurityType::Equity {
        if let Some(rows) = context.cached_auxiliary_map_rows(&auxiliary_key) {
            rows
        } else {
            let rows = provider
                .get_map_file(&config.symbol)
                .await
                .map_err(data_error)?
                .unwrap_or_default();
            context.cache_auxiliary_map_rows(auxiliary_key, rows.clone());
            rows
        }
    } else {
        Vec::new()
    };
    produce_native(
        &config,
        &context,
        provider,
        (start, end),
        (&factor_rows, &map_rows),
        sender,
        &mut cancelled,
    )
    .await
}

async fn produce_native(
    config: &SubscriptionDataConfig,
    context: &DataFeedContext,
    provider: Arc<dyn HistoricalDataProvider>,
    range: (DateTime, DateTime),
    auxiliary: (&[FactorFileEntry], &[rlean_data_tables::MapFileEntry]),
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
    cancelled: &mut oneshot::Receiver<()>,
) -> LeanResult<()> {
    let (start, end) = range;
    let mut window_start = start.date_utc();
    let end_date = effective_subscription_end(config, end.date_utc());
    let mut last_point: Option<SubscriptionDataPoint> = None;
    let mut last_real_frontier: Option<DateTime> = None;

    while window_start <= end_date {
        let window_end = backtest_window_candidate(config.resolution, window_start).min(end_date);
        let (query_start, query_end) = bounded_query_times(window_start, window_end, start, end);
        let mut query_config = config.clone();
        if let Some(mapped) = auxiliary.1.iter().find(|row| row.date >= window_start) {
            query_config.symbol = config.symbol.with_mapped_value(&mapped.mapped_symbol);
        }
        let request =
            HistoryRequest::new(query_config, query_start, query_end).map_err(data_error)?;
        let data = tokio::select! {
            _ = &mut *cancelled => return Ok(()),
            result = provider.get_history(&request) => result.map_err(data_error)?,
        };
        let mut points = native_points(config, data, auxiliary.0)?;
        points.sort_by_key(SubscriptionDataPoint::frontier_time);
        points = deduplicate_points(config, points);
        let prefetch_requests = option_chain_prefetch_requests(config, &points)?;
        if !prefetch_requests.is_empty() {
            tokio::select! {
                _ = &mut *cancelled => return Ok(()),
                result = provider.prefetch_history(&prefetch_requests) => result.map_err(data_error)?,
            }
        }
        tracing::debug!(
            provider = provider.name(),
            symbol = %config.symbol.value,
            window_start = %window_start,
            window_end = %window_end,
            points = points.len(),
            "decoded native historical-provider window"
        );
        for point in points {
            let frontier = point.frontier_time();
            if frontier < start || frontier > end {
                continue;
            }
            // LEAN SubscriptionDataReader permits equal EndTime values for
            // custom data because they can be independent events. Ordinary
            // non-tick market data still rejects equal frontiers.
            let out_of_order_or_duplicate = last_real_frontier
                .map(|last| {
                    if config.data_kind == rlean_data::SubscriptionDataKind::Custom {
                        frontier < last
                    } else {
                        frontier <= last
                    }
                })
                .unwrap_or(false);
            if out_of_order_or_duplicate && !config.resolution.is_tick() {
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

        let watermark = query_end;
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

fn option_chain_prefetch_requests(
    config: &SubscriptionDataConfig,
    points: &[SubscriptionDataPoint],
) -> LeanResult<Vec<HistoryRequest>> {
    if config.option_chain.is_none() || config.resolution != Resolution::Minute {
        return Ok(Vec::new());
    }
    let frontiers = points.iter().filter_map(|point| match point {
        SubscriptionDataPoint::OptionChain { frontier_time, .. } => Some(*frontier_time),
        _ => None,
    });
    let Some(start) = frontiers.clone().min() else {
        return Ok(Vec::new());
    };
    let end = frontiers.max().unwrap_or(start) + TimeSpan::from_nanos(1);
    let mut requests = Vec::new();
    let mut seen = HashSet::new();
    for point in points {
        let SubscriptionDataPoint::OptionChain { chain, .. } = point else {
            continue;
        };
        for symbol in chain.contracts.keys() {
            for tick_type in [TickType::Trade, TickType::Quote] {
                let mut market_config =
                    SubscriptionDataConfig::new_option(symbol.clone(), config.resolution);
                market_config.set_tick_type(tick_type);
                if !seen.insert(market_config.unique_id()) {
                    continue;
                }
                requests.push(HistoryRequest::new(market_config, start, end).map_err(data_error)?);
            }
        }
    }
    Ok(requests)
}

fn native_points(
    config: &SubscriptionDataConfig,
    data: HistoricalData,
    factor_rows: &[FactorFileEntry],
) -> LeanResult<Vec<SubscriptionDataPoint>> {
    match data {
        HistoricalData::TradeBars(rows) => Ok(rows
            .into_iter()
            .filter_map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                normalize_trade_bar(&mut bar, config.normalization_mode, factor_rows);
                bar.is_valid()
                    .then_some(SubscriptionDataPoint::TradeBar(bar))
            })
            .collect()),
        HistoricalData::QuoteBars(rows) => Ok(rows
            .into_iter()
            .map(|mut bar| {
                bar.venue.get_or_insert_with(|| config.venue.clone());
                normalize_quote_bar(&mut bar, config.normalization_mode, factor_rows);
                SubscriptionDataPoint::QuoteBar(bar)
            })
            .collect()),
        HistoricalData::Ticks(rows) => Ok(rows
            .into_iter()
            .map(|mut tick| {
                tick.venue.get_or_insert_with(|| config.venue.clone());
                SubscriptionDataPoint::Tick(tick)
            })
            .collect()),
        HistoricalData::CustomPoints(rows) => {
            let metadata = config.custom.as_ref().ok_or_else(|| {
                LeanError::DataError("custom history response has no subscription metadata".into())
            })?;
            Ok(rows
                .into_iter()
                .map(|point| SubscriptionDataPoint::CustomData {
                    symbol: config.symbol.clone(),
                    ticker: metadata.ticker.clone(),
                    point,
                })
                .collect())
        }
        HistoricalData::OptionUniverse(rows) => {
            let chains = crate::option_universe::option_chains_from_rows(config, rows)
                .map_err(data_error)?;
            Ok(chains
                .into_iter()
                .filter_map(|(date, chain)| {
                    let frontier_time = option_chain_frontier(config, date)?;
                    Some(SubscriptionDataPoint::OptionChain {
                        canonical_permtick: config
                            .option_chain
                            .as_ref()?
                            .canonical_permtick
                            .clone(),
                        chain: std::sync::Arc::new(chain),
                        frontier_time,
                    })
                })
                .collect())
        }
        HistoricalData::FundamentalUniverse(rows) => {
            let mut by_frontier = std::collections::BTreeMap::new();
            for row in rows {
                let frontier_time = rlean_core::NanosecondTimestamp(
                    row.end_time
                        .and_utc()
                        .timestamp_nanos_opt()
                        .unwrap_or_default(),
                );
                let time = rlean_core::NanosecondTimestamp(
                    row.time.and_utc().timestamp_nanos_opt().unwrap_or_default(),
                );
                let mut point = rlean_data::FundamentalData::new(
                    rlean_core::Symbol::create_equity(
                        &row.symbol_value,
                        &rlean_core::Market::new(&row.market),
                    ),
                    time,
                );
                point.end_time = frontier_time;
                point.volume = Some(row.volume);
                point.dollar_volume = Some(row.dollar_volume);
                point.market_cap = Some(row.market_cap);
                by_frontier
                    .entry(frontier_time)
                    .or_insert_with(Vec::new)
                    .push(point);
            }
            Ok(by_frontier
                .into_iter()
                .map(
                    |(frontier_time, data)| SubscriptionDataPoint::FundamentalUniverse {
                        data,
                        frontier_time,
                    },
                )
                .collect())
        }
        HistoricalData::FutureUniverse(_) => Err(LeanError::DataError(
            "future-universe delivery is not yet supported by the engine".to_string(),
        )),
    }
}

/// Keep partition-aligned interior windows while preserving the caller's exact
/// first and final frontiers. In particular, a live last-known-price request
/// ending during an open session must not be widened to 23:59:59: C# LEAN ends
/// the history request at algorithm time, so the unfinished Daily bar is not
/// available yet.
fn bounded_query_times(
    window_start: chrono::NaiveDate,
    window_end: chrono::NaiveDate,
    requested_start: DateTime,
    requested_end: DateTime,
) -> (DateTime, DateTime) {
    (
        partition_day_start(window_start).max(requested_start),
        partition_day_end(window_end).min(requested_end),
    )
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
    // LEAN's FillForwardEnumerator calls Exchange.IsOpenDuringBar for every
    // market-data security type. Treating non-equities as always open creates
    // synthetic option bars on weekends and advances the algorithm on dates
    // when the option exchange is closed.
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
        // request per trading day.
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

/// LEAN's `BaseChainUniverseData.EndTime` is the following midnight in the
/// option exchange's time zone. The synchronizer operates in UTC, so emitting
/// UTC midnight would deliver a US chain on the prior local calendar date and
/// make same-day expiry filters see an empty chain.
fn option_chain_frontier(
    config: &SubscriptionDataConfig,
    source_date: chrono::NaiveDate,
) -> Option<DateTime> {
    let frontier_date = source_date.succ_opt()?;
    let exchange_symbol = config
        .symbol
        .underlying
        .as_deref()
        .unwrap_or(&config.symbol);
    MarketHoursDatabase::global()
        .exchange_hours(exchange_symbol)
        .local_midnight_utc(frontier_date)
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
    // Preserve anyhow's full context chain at the boundary where provider
    // errors become the string-backed LEAN error contract. Formatting with
    // `{:#}` is identical for ordinary Display values and includes every
    // source for `anyhow::Error`, which keeps storage/provider failures
    // actionable after crossing this boundary.
    LeanError::DataError(format!("{error:#}"))
}

use chrono::Datelike;

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use rlean_core::{Market, OptionRight, OptionStyle, Symbol, TimeSpan};
    use rlean_data::{
        CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata, OptionChainFilterMetadata,
        OptionChainSubscriptionMetadata,
    };
    use rlean_data_tables::{CustomDataPoint, TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    #[test]
    fn data_errors_preserve_the_provider_error_chain() {
        let error = Err::<(), _>(anyhow::anyhow!("HTTP 409 commit conflict"))
            .context("persist successful historical coverage")
            .unwrap_err();

        let rendered = data_error(error).to_string();
        assert!(rendered.contains("persist successful historical coverage"));
        assert!(rendered.contains("HTTP 409 commit conflict"));
    }

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
    fn option_chain_frontier_is_following_exchange_local_midnight() {
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

        let frontier = option_chain_frontier(
            &config,
            chrono::NaiveDate::from_ymd_opt(2024, 7, 18).unwrap(),
        )
        .unwrap();
        assert_eq!(
            frontier,
            DateTime::from(
                chrono::NaiveDate::from_ymd_opt(2024, 7, 19)
                    .unwrap()
                    .and_hms_opt(4, 0, 0)
                    .unwrap()
            )
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
    fn bounded_query_preserves_partial_day_history_frontiers() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let requested_start = DateTime::from(date.and_hms_opt(13, 30, 0).unwrap());
        let requested_end = DateTime::from(date.and_hms_opt(16, 52, 0).unwrap());

        let (query_start, query_end) =
            bounded_query_times(date, date, requested_start, requested_end);

        assert_eq!(query_start, requested_start);
        assert_eq!(query_end, requested_end);
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

    #[tokio::test]
    async fn option_fill_forward_does_not_cross_a_closed_weekend() {
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let option = Symbol::create_option(
            underlying,
            &Market::usa(),
            chrono::NaiveDate::from_ymd_opt(2024, 7, 22).unwrap(),
            dec!(550),
            OptionRight::Call,
            OptionStyle::American,
        );
        let mut config = SubscriptionDataConfig::new_option(option.clone(), Resolution::Minute);
        config.fill_data_forward = true;
        let period = TimeSpan::ONE_MINUTE;
        let friday_close = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 7, 19)
                .unwrap()
                .and_hms_opt(20, 0, 0)
                .unwrap(),
        );
        let previous = SubscriptionDataPoint::TradeBar(TradeBar::new(
            option,
            friday_close - period,
            period,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(1)),
        ));
        let monday_open = DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 7, 22)
                .unwrap()
                .and_hms_opt(13, 31, 0)
                .unwrap(),
        );
        let (sender, _receiver) = mpsc::channel(128);

        assert!(send_fill_forward_before(
            &config,
            MarketHoursDatabase::global().as_ref(),
            &previous,
            monday_open,
            monday_open,
            &sender,
        )
        .await
        .unwrap()
        .is_none());
    }
}
