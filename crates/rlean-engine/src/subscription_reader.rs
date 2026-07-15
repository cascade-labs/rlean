use crate::data_feed::DataFeedContext;
use crate::normalization::{normalize_quote_bar, normalize_trade_bar};
use crate::subscription_data::SubscriptionDataPoint;
use futures::StreamExt;
use rlean_core::{DateTime, LeanError, Resolution, Result as LeanResult, SecurityType};
use rlean_data::SubscriptionDataConfig;
use rlean_data_sidecar::{
    decode_batch, decode_factor_file_batch, decode_map_file_batch, CanonicalDataBatch,
    DeliveryMode, SubscriptionSpec, WireDataType,
};
use rlean_data_tables::FactorFileEntry;
use std::collections::{BTreeMap, VecDeque};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

enum SubscriptionStreamMessage {
    Point(SubscriptionDataPoint),
    Watermark(DateTime),
}

/// One engine subscription backed by one registered sidecar subscription.
///
/// The producer requests bounded backtest windows and the channel supplies the
/// same backpressure that the synchronizer used before the Flight migration.
/// There is no storage/provider fallback: a sidecar session is mandatory.
pub struct SubscriptionStream {
    config: SubscriptionDataConfig,
    receiver: mpsc::Receiver<LeanResult<SubscriptionStreamMessage>>,
    producer: JoinHandle<()>,
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
        let capacity = context.effective_buffer_policy().channel_capacity();
        let (sender, receiver) = mpsc::channel(capacity);
        let producer_config = config.clone();
        let producer = tokio::spawn(async move {
            if let Err(error) = produce(producer_config, context, start, end, &sender).await {
                let _ = sender.send(Err(error)).await;
            }
        });
        Self {
            config,
            receiver,
            producer,
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

    pub fn is_ready_for(&self, frontier: DateTime) -> bool {
        self.exhausted
            || !self.pending.is_empty()
            || self
                .watermark
                .map(|watermark| watermark > frontier)
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
            Some(Ok(message)) => self.handle_message(message),
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
                Ok(Ok(message)) => self.handle_message(message),
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

    fn handle_message(&mut self, message: SubscriptionStreamMessage) {
        match message {
            SubscriptionStreamMessage::Point(point) => self.pending.push_back(point),
            SubscriptionStreamMessage::Watermark(watermark) => {
                if self
                    .watermark
                    .map(|current| watermark > current)
                    .unwrap_or(true)
                {
                    self.watermark = Some(watermark);
                }
            }
        }
    }
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

async fn produce(
    config: SubscriptionDataConfig,
    context: DataFeedContext,
    start: DateTime,
    end: DateTime,
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<()> {
    let sidecar = &context.sidecar;
    let subscription_id = sidecar
        .add_subscription(&config, DeliveryMode::Backtest)
        .await
        .map_err(data_error)?;
    let factor_rows = match load_auxiliary_rows(&config, &context).await {
        Ok(rows) => rows,
        Err(error) => {
            let _ = sidecar.remove_subscription(subscription_id).await;
            return Err(error);
        }
    };
    let result = produce_registered(
        &config,
        &context,
        subscription_id,
        start,
        end,
        &factor_rows,
        sender,
    )
    .await;
    if let Err(error) = sidecar.remove_subscription(subscription_id).await {
        tracing::warn!(subscription_id, %error, "failed to remove sidecar subscription");
    }
    result
}

async fn load_auxiliary_rows(
    config: &SubscriptionDataConfig,
    context: &DataFeedContext,
) -> LeanResult<Vec<FactorFileEntry>> {
    if config.symbol.security_type() != SecurityType::Equity {
        return Ok(Vec::new());
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

    let mut map_spec = SubscriptionSpec::from(config);
    map_spec.config_id ^= 0x6d61_7000_0000_0000;
    map_spec.data_type = WireDataType::MapFile as i32;
    map_spec.resolution = 4;
    map_spec.tick_type = 0;
    let map_batches = query_auxiliary(context, map_spec).await?;
    let mut map_rows = Vec::new();
    for batch in &map_batches {
        map_rows.extend(decode_map_file_batch(batch).map_err(data_error)?);
    }
    map_rows.sort_by_key(|row| row.date);

    tracing::info!(
        ticker = %config.symbol.permtick,
        factor_rows = factor_rows.len(),
        map_rows = map_rows.len(),
        "loaded equity auxiliary data from sidecar"
    );
    if config.normalization_mode != rlean_core::DataNormalizationMode::Raw && factor_rows.is_empty()
    {
        context.record_unadjusted_equity(config.symbol.permtick.as_ref());
    }
    Ok(factor_rows)
}

async fn query_auxiliary(
    context: &DataFeedContext,
    spec: SubscriptionSpec,
) -> LeanResult<Vec<arrow::record_batch::RecordBatch>> {
    let sidecar = &context.sidecar;
    let subscription_id = sidecar
        .add_subscription_spec(spec, DeliveryMode::Backtest)
        .await
        .map_err(data_error)?;
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
    start: DateTime,
    end: DateTime,
    factor_rows: &[FactorFileEntry],
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<()> {
    let wire_type = WireDataType::try_from(SubscriptionSpec::from(config).data_type)
        .map_err(|value| LeanError::DataError(format!("unknown sidecar data type {value}")))?;
    let mut window_start = start.date_utc();
    let end_date = end.date_utc();
    let mut last_point: Option<SubscriptionDataPoint> = None;
    let mut last_real_frontier: Option<DateTime> = None;

    while window_start <= end_date {
        wait_for_prefetch_horizon(context, window_start).await;
        let window_end = backtest_window_end(config.resolution, window_start, end_date, context);
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
            points.extend(decode_points(config, wire_type, batch, factor_rows)?);
        }
        // Flight is free to split one daily cross-section across record
        // batches. Reassemble all pieces before the synchronizer sees it so a
        // universe selector always receives the complete snapshot.
        points = coalesce_fundamental_snapshots(points);
        points.sort_by_key(SubscriptionDataPoint::frontier_time);
        points = deduplicate_points(config, points);

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
                send_fill_forward(config, context, previous, frontier, end, sender).await?;
            }
            if sender
                .send(Ok(SubscriptionStreamMessage::Point(point.clone())))
                .await
                .is_err()
            {
                return Ok(());
            }
            last_real_frontier = Some(frontier);
            last_point = Some(point);
        }

        if sender
            .send(Ok(SubscriptionStreamMessage::Watermark(partition_day_end(
                window_end,
            ))))
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
        CanonicalDataBatch::RecordBatch(_) => Err(LeanError::DataError(format!(
            "sidecar data type {wire_type:?} is not consumable by a subscription"
        ))),
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

async fn send_fill_forward(
    config: &SubscriptionDataConfig,
    context: &DataFeedContext,
    previous: &SubscriptionDataPoint,
    next_real_frontier: DateTime,
    end: DateTime,
    sender: &mpsc::Sender<LeanResult<SubscriptionStreamMessage>>,
) -> LeanResult<()> {
    if config.resolution.is_tick() || !config.fill_data_forward {
        return Ok(());
    }
    let Some(period) = config.resolution.to_time_span() else {
        return Ok(());
    };
    let mut frontier = previous.frontier_time() + period;
    while frontier < next_real_frontier && frontier <= end {
        if is_market_open(config, context, frontier, period) {
            if let Some(fill) = fill_forward_point(previous, frontier, period) {
                if sender
                    .send(Ok(SubscriptionStreamMessage::Point(fill)))
                    .await
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
        frontier = frontier + period;
    }
    Ok(())
}

fn is_market_open(
    config: &SubscriptionDataConfig,
    context: &DataFeedContext,
    frontier: DateTime,
    period: rlean_core::TimeSpan,
) -> bool {
    if config.symbol.security_type() != SecurityType::Equity {
        return true;
    }
    context
        .market_hours_database
        .exchange_hours(&config.symbol)
        .is_open_at(frontier - period)
}

async fn wait_for_prefetch_horizon(context: &DataFeedContext, date: chrono::NaiveDate) {
    loop {
        let beyond = context
            .prefetch_ceiling_date()
            .map(|ceiling| date > ceiling)
            .unwrap_or(false);
        if !beyond {
            return;
        }
        let notified = context.frontier_advanced();
        let still_beyond = context
            .prefetch_ceiling_date()
            .map(|ceiling| date > ceiling)
            .unwrap_or(false);
        if still_beyond {
            notified.await;
        }
    }
}

fn backtest_window_end(
    resolution: Resolution,
    start: chrono::NaiveDate,
    backtest_end: chrono::NaiveDate,
    context: &DataFeedContext,
) -> chrono::NaiveDate {
    let candidate = backtest_window_candidate(resolution, start);
    let horizon = context.prefetch_ceiling_date().unwrap_or(backtest_end);
    candidate.min(backtest_end).min(horizon)
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
    use rlean_core::{Market, Symbol, TimeSpan};
    use rlean_data::{CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata};
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
}
