use crate::data_feed::DataFeedContext;
use crate::normalization::{normalize_quote_bar, normalize_trade_bar, read_factor_rows};
use crate::options_service::{
    build_daily_eod_chain, option_underlying_ticker, underlying_price_from_bars,
};
use crate::subscription_data::SubscriptionDataPoint;
use anyhow::Context;
use chrono::Datelike;
use lean_core::{
    exchange_hours::ExchangeHours, DateTime, Resolution, Result as LeanResult, SecurityType,
    Symbol, TickType,
};
use lean_data::{
    CustomDataQuery, CustomDataSource, CustomDataTransport, OptionChainSubscriptionMetadata,
    QuoteBar, SubscriptionDataConfig, SubscriptionDataKind, Tick, TradeBar,
};
use lean_data_providers::{DataType, HistoryRequest, ICustomDataSource};
use lean_storage::{FactorFileEntry, OptionEodBar, QueryParams};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferedPointState {
    Loaded,
    Emittable,
}

#[derive(Debug, Clone)]
struct BufferedSubscriptionPoint {
    point: SubscriptionDataPoint,
    state: BufferedPointState,
}

struct LoadedPartition {
    points: Vec<SubscriptionDataPoint>,
    through_date: chrono::NaiveDate,
}

impl BufferedSubscriptionPoint {
    fn loaded(point: SubscriptionDataPoint) -> Self {
        Self {
            point,
            state: BufferedPointState::Loaded,
        }
    }

    fn mark_emittable(&mut self) {
        self.state = BufferedPointState::Emittable;
    }

    fn frontier_time(&self) -> DateTime {
        self.point.frontier_time()
    }
}

/// LEAN-style subscription reader: advances partition sources by date and emits
/// ordered, normalized data points for one subscription config.
pub struct SubscriptionStream {
    config: SubscriptionDataConfig,
    receiver: mpsc::Receiver<LeanResult<SubscriptionDataPoint>>,
    producer: JoinHandle<()>,
    pending: VecDeque<SubscriptionDataPoint>,
    exhausted: bool,
    producer_error: Option<lean_core::LeanError>,
}

struct SubscriptionProducerState {
    config: SubscriptionDataConfig,
    context: DataFeedContext,
    factor_rows: Vec<FactorFileEntry>,
    start: DateTime,
    end: DateTime,
    partition_date: chrono::NaiveDate,
    end_partition_date: chrono::NaiveDate,
    cache_filled_until: Option<chrono::NaiveDate>,
    pending: VecDeque<BufferedSubscriptionPoint>,
    prefetched: BTreeMap<DateTime, VecDeque<BufferedSubscriptionPoint>>,
    last_emitted_time: Option<DateTime>,
    last_emitted_point: Option<SubscriptionDataPoint>,
    daily_time_is_source_date: Option<bool>,
    exhausted: bool,
}

impl SubscriptionStream {
    pub fn new(
        config: SubscriptionDataConfig,
        context: DataFeedContext,
        start: DateTime,
        end: DateTime,
    ) -> Self {
        let capacity = context
            .options
            .cache_policy
            .max_prefetch_rows
            .clamp(1, 100_000);
        let (sender, receiver) = mpsc::channel(capacity);
        let producer_config = config.clone();
        let producer = tokio::spawn(async move {
            let state = SubscriptionProducerState::new(producer_config, context, start, end);
            state.run(sender).await;
        });
        Self {
            config,
            receiver,
            producer,
            pending: VecDeque::new(),
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

    pub async fn fill_pending(&mut self) -> LeanResult<()> {
        if let Some(error) = self.producer_error.take() {
            return Err(error);
        }
        if !self.pending.is_empty() || self.exhausted {
            return Ok(());
        }
        self.drain_available_messages()?;
        if !self.pending.is_empty() || self.exhausted {
            return Ok(());
        }
        match self.receiver.recv().await {
            Some(Ok(point)) => self.pending.push_back(point),
            Some(Err(error)) => {
                self.exhausted = true;
                return Err(error);
            }
            None => self.exhausted = true,
        }
        self.drain_available_messages()?;
        Ok(())
    }

    pub async fn pop_next(&mut self) -> LeanResult<Option<SubscriptionDataPoint>> {
        self.fill_pending().await?;
        let next = self.pending.pop_front();
        if self.pending.is_empty() {
            self.drain_available_messages()?;
        }
        Ok(next)
    }

    fn drain_available_messages(&mut self) -> LeanResult<()> {
        loop {
            match self.receiver.try_recv() {
                Ok(Ok(point)) => self.pending.push_back(point),
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
}

impl Drop for SubscriptionStream {
    fn drop(&mut self) {
        self.producer.abort();
    }
}

impl SubscriptionProducerState {
    fn new(
        config: SubscriptionDataConfig,
        context: DataFeedContext,
        start: DateTime,
        end: DateTime,
    ) -> Self {
        let factor_rows = if config.normalization_mode != lean_core::DataNormalizationMode::Raw {
            read_factor_rows(&context.store, &config.symbol)
        } else {
            Vec::new()
        };
        let partition_date = start.date_utc();
        let end_partition_date = end.date_utc();
        Self {
            config,
            context,
            factor_rows,
            start,
            end,
            partition_date,
            end_partition_date,
            cache_filled_until: None,
            pending: VecDeque::new(),
            prefetched: BTreeMap::new(),
            last_emitted_time: None,
            last_emitted_point: None,
            daily_time_is_source_date: None,
            exhausted: false,
        }
    }

    async fn run(mut self, sender: mpsc::Sender<LeanResult<SubscriptionDataPoint>>) {
        if let Err(error) = self.run_inner(&sender).await {
            let _ = sender.send(Err(error)).await;
        }
    }

    async fn run_inner(
        &mut self,
        sender: &mpsc::Sender<LeanResult<SubscriptionDataPoint>>,
    ) -> LeanResult<()> {
        while !self.exhausted {
            if self.promote_next_frontier() {
                self.send_pending(sender).await?;
                continue;
            }
            if self.partition_date > self.end_partition_date {
                self.exhausted = true;
                break;
            }
            if self.should_skip_partition_date() {
                self.partition_date += chrono::Duration::days(1);
                continue;
            }
            let loaded = self.load_partition().await?;
            if let Some(next_frontier) = loaded
                .points
                .iter()
                .map(SubscriptionDataPoint::frontier_time)
                .min()
            {
                self.maybe_stage_fill_forward(next_frontier);
            }
            self.stage_loaded_points(loaded.points);
            self.advance_partition_past(loaded.through_date);
        }
        Ok(())
    }

    async fn send_pending(
        &mut self,
        sender: &mpsc::Sender<LeanResult<SubscriptionDataPoint>>,
    ) -> LeanResult<()> {
        while let Some(buffered) = self.pending.pop_front() {
            let frontier_time = buffered.frontier_time();
            let point = buffered.point;
            self.last_emitted_time = Some(frontier_time);
            self.last_emitted_point = Some(point.clone());
            if sender.send(Ok(point)).await.is_err() {
                self.exhausted = true;
                break;
            }
        }
        Ok(())
    }

    fn advance_partition_past(&mut self, through_date: chrono::NaiveDate) {
        let next_date = through_date.succ_opt().unwrap_or(through_date);
        if next_date > self.partition_date {
            self.partition_date = next_date;
        } else {
            self.partition_date += chrono::Duration::days(1);
        }
    }

    async fn load_partition(&mut self) -> LeanResult<LoadedPartition> {
        if self.config.data_kind == SubscriptionDataKind::Option {
            return self.load_option_partition().await;
        }
        if self.config.data_kind != SubscriptionDataKind::Market {
            return self.load_custom_partition().await;
        }
        let (window_start, window_end) = self.cache_fill_window();
        let fetched_points = match self.fetch_market_window_if_missing().await {
            Ok(points) => points,
            Err(error) => {
                tracing::warn!(
                    "failed to fetch market data window {} on {}: {:#}",
                    self.config.symbol.value,
                    self.partition_date,
                    error
                );
                Vec::new()
            }
        };
        let day_start = partition_day_start(window_start);
        let day_end = partition_day_end(window_end);
        let params = if self.config.resolution == Resolution::Daily {
            daily_market_query_params(self.config.symbol.id.sid, window_start, window_end)
        } else if self.config.resolution.is_tick() {
            QueryParams::new()
                .with_time_range(day_start, day_end)
                .with_symbols(vec![self.config.symbol.id.sid])
        } else {
            QueryParams::new()
                .with_day_range(day_start, day_end)
                .with_bar_range(day_start, day_end)
                .with_symbols(vec![self.config.symbol.id.sid])
        };

        let mut points = if self.config.resolution.is_tick() {
            self.load_ticks(&params).await?
        } else if self.config.tick_type == TickType::Quote {
            self.load_quote_bars(&params, window_start, window_end)
                .await?
        } else {
            self.load_trade_bars(&params, window_start, window_end)
                .await?
        };
        if points.is_empty() && !fetched_points.is_empty() {
            points = fetched_points;
        }

        let mut out = Vec::new();
        for point in points {
            let frontier_time = point.frontier_time();
            if frontier_time > self.end {
                continue;
            }
            if self.config.resolution == Resolution::Daily
                && self.config.symbol.security_type() == SecurityType::Equity
            {
                let exchange_hours = self
                    .context
                    .market_hours_database
                    .exchange_hours(&self.config.symbol);
                if daily_point_coverage_date(&point, &exchange_hours)
                    .map(|date| date >= window_start && date <= window_end)
                    .unwrap_or(false)
                {
                    out.push(point);
                }
            } else if frontier_time >= self.start {
                out.push(point);
            }
        }
        let through_date = self
            .cache_filled_until
            .filter(|date| *date >= window_start && *date <= window_end)
            .unwrap_or(window_end);
        Ok(LoadedPartition {
            points: out,
            through_date,
        })
    }

    fn stage_loaded_points(&mut self, points: Vec<SubscriptionDataPoint>) {
        let max_rows = self.context.options.cache_policy.max_prefetch_rows;
        for point in points {
            let frontier_time = point.frontier_time();
            if self
                .last_emitted_time
                .map(|last| frontier_time <= last)
                .unwrap_or(false)
            {
                tracing::warn!(
                    "dropping non-monotonic subscription point for {} at {:?}",
                    self.config.symbol.value,
                    frontier_time
                );
                continue;
            }
            if self.should_skip_duplicate_frontier(&point)
                && self.has_buffered_frontier(frontier_time)
            {
                continue;
            }
            let staged_rows: usize = self.prefetched.values().map(VecDeque::len).sum();
            if staged_rows >= max_rows {
                tracing::warn!(
                    "subscription prefetch buffer for {} reached {} rows; keeping stream bounded",
                    self.config.symbol.value,
                    max_rows
                );
                break;
            }
            self.prefetched
                .entry(frontier_time)
                .or_default()
                .push_back(BufferedSubscriptionPoint::loaded(point));
        }
    }

    fn should_skip_duplicate_frontier(&self, point: &SubscriptionDataPoint) -> bool {
        if self.config.resolution.is_tick() {
            return false;
        }
        matches!(
            point,
            SubscriptionDataPoint::TradeBar(_) | SubscriptionDataPoint::QuoteBar(_)
        )
    }

    fn has_buffered_frontier(&self, frontier_time: DateTime) -> bool {
        self.pending
            .iter()
            .any(|buffered| buffered.frontier_time() == frontier_time)
            || self
                .prefetched
                .get(&frontier_time)
                .map(|rows| !rows.is_empty())
                .unwrap_or(false)
    }

    fn promote_next_frontier(&mut self) -> bool {
        if !self.pending.is_empty() {
            return true;
        }
        let Some(frontier_time) = self.prefetched.keys().next().copied() else {
            return false;
        };
        let Some(mut points) = self.prefetched.remove(&frontier_time) else {
            return false;
        };
        for point in points.iter_mut() {
            point.mark_emittable();
        }
        self.pending.extend(points);
        true
    }

    fn maybe_stage_fill_forward(&mut self, next_real_frontier: DateTime) {
        if self.config.resolution.is_tick() || !self.config.fill_data_forward {
            return;
        }
        let Some(period) = self.config.resolution.to_time_span() else {
            return;
        };
        let Some(last_point) = self.last_emitted_point.as_ref() else {
            return;
        };
        let fill_frontier = last_point.frontier_time() + period;
        if fill_frontier >= next_real_frontier || fill_frontier > self.end {
            return;
        }
        if self.config.resolution == Resolution::Daily
            && self.config.symbol.security_type() == SecurityType::Equity
            && !is_open_equity_date(
                &self
                    .context
                    .market_hours_database
                    .exchange_hours(&self.config.symbol),
                fill_frontier.date_utc(),
            )
        {
            return;
        }
        if !self.is_regular_market_bar(fill_frontier) {
            return;
        }
        let Some(fill_point) = fill_forward_point(last_point, fill_frontier, period) else {
            return;
        };
        self.stage_loaded_points(vec![fill_point]);
    }

    async fn fetch_market_window_if_missing(
        &mut self,
    ) -> anyhow::Result<Vec<SubscriptionDataPoint>> {
        let Some(provider) = self.context.history_provider.as_ref() else {
            return Ok(Vec::new());
        };
        if self
            .cache_filled_until
            .map(|date| self.partition_date <= date)
            .unwrap_or(false)
        {
            return Ok(Vec::new());
        }

        let data_type = if self.config.resolution.is_tick() {
            DataType::Tick
        } else if self.config.tick_type == TickType::Quote {
            DataType::QuoteBar
        } else {
            DataType::TradeBar
        };
        let (window_start, window_end) = self.cache_fill_window();
        if self
            .has_local_market_window(window_start, window_end)
            .await?
        {
            self.cache_filled_until = Some(window_end);
            return Ok(Vec::new());
        }
        let request = HistoryRequest {
            symbol: self.config.symbol.clone(),
            resolution: self.config.resolution,
            start: partition_day_start(window_start),
            end: partition_day_end(window_end),
            data_type,
        };

        let mut cache_filled_until = Some(window_end);
        let points = match data_type {
            DataType::TradeBar => {
                let rows = provider
                    .get_history(&request)
                    .await?
                    .into_iter()
                    .filter(|row| self.trade_bar_belongs_to_window(row, window_start, window_end))
                    .collect::<Vec<_>>();
                if self.config.resolution == Resolution::Daily {
                    cache_filled_until =
                        self.daily_fetched_trade_bars_until(&rows, window_start, window_end);
                }
                self.context
                    .store
                    .append_trade_bars_unchecked(
                        &rows,
                        self.config.symbol.security_type(),
                        self.config.symbol.market().as_str(),
                        self.config.resolution,
                        self.config.tick_type,
                    )
                    .await?;
                self.trade_bars_to_points(rows, window_start, window_end)?
            }
            DataType::QuoteBar => {
                let rows = provider
                    .get_quote_bars(&request)
                    .await?
                    .into_iter()
                    .filter(|row| self.quote_bar_belongs_to_window(row, window_start, window_end))
                    .collect::<Vec<_>>();
                if self.config.resolution == Resolution::Daily {
                    cache_filled_until =
                        self.daily_fetched_quote_bars_until(&rows, window_start, window_end);
                }
                self.context
                    .store
                    .append_quote_bars_unchecked(
                        &rows,
                        self.config.symbol.security_type(),
                        self.config.symbol.market().as_str(),
                        self.config.resolution,
                        self.config.tick_type,
                    )
                    .await?;
                self.quote_bars_to_points(rows, window_start, window_end)?
            }
            DataType::Tick => {
                let rows = provider
                    .get_ticks(&request)
                    .await?
                    .into_iter()
                    .filter(|row| self.tick_belongs_to_window(row, window_start, window_end))
                    .collect::<Vec<_>>();
                self.context
                    .store
                    .append_ticks(
                        &rows,
                        self.config.symbol.security_type(),
                        self.config.symbol.market().as_str(),
                        self.config.resolution,
                        self.config.tick_type,
                    )
                    .await?;
                rows.into_iter().map(SubscriptionDataPoint::Tick).collect()
            }
            _ => Vec::new(),
        };
        self.cache_filled_until = cache_filled_until;
        Ok(points)
    }

    fn daily_fetched_trade_bars_until(
        &self,
        rows: &[TradeBar],
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> Option<chrono::NaiveDate> {
        let expected = expected_market_dates(
            window_start,
            window_end,
            self.end_partition_date,
            self.exchange_hours().as_deref(),
            self.config.symbol.security_type(),
        );
        let exchange_hours = self
            .context
            .market_hours_database
            .exchange_hours(&self.config.symbol);
        let covered = fetched_daily_trade_bar_dates(rows, window_start, &expected, &exchange_hours);
        contiguous_covered_until(
            window_start,
            window_end,
            self.end_partition_date,
            &expected,
            &covered,
        )
    }

    fn daily_fetched_quote_bars_until(
        &self,
        rows: &[QuoteBar],
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> Option<chrono::NaiveDate> {
        let expected = expected_market_dates(
            window_start,
            window_end,
            self.end_partition_date,
            self.exchange_hours().as_deref(),
            self.config.symbol.security_type(),
        );
        let exchange_hours = self
            .context
            .market_hours_database
            .exchange_hours(&self.config.symbol);
        let covered = fetched_daily_quote_bar_dates(rows, window_start, &expected, &exchange_hours);
        contiguous_covered_until(
            window_start,
            window_end,
            self.end_partition_date,
            &expected,
            &covered,
        )
    }

    async fn has_local_market_window(
        &self,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> anyhow::Result<bool> {
        let day_start = partition_day_start(window_start);
        let day_end = partition_day_end(window_end);
        let expected = expected_market_dates(
            window_start,
            window_end,
            self.end_partition_date,
            self.exchange_hours().as_deref(),
            self.config.symbol.security_type(),
        );
        if expected.is_empty() {
            return Ok(true);
        }

        if self.config.resolution != Resolution::Daily {
            let table = if self.config.resolution.is_tick() {
                lean_storage::iceberg_store::MARKET_TICKS
            } else if self.config.tick_type == TickType::Quote {
                lean_storage::iceberg_store::MARKET_QUOTE_BARS
            } else {
                lean_storage::iceberg_store::MARKET_TRADE_BARS
            };
            let available = self
                .context
                .store
                .market_partition_days(
                    table,
                    self.config.symbol.security_type(),
                    self.config.symbol.market().as_str(),
                    self.config.resolution,
                    self.config.symbol.id.sid,
                    days_since_epoch(day_start.0),
                    days_since_epoch(day_end.0),
                )
                .await?
                .into_iter()
                .filter_map(date_from_days_since_epoch)
                .collect::<HashSet<_>>();
            return Ok(expected.is_subset(&available));
        }

        let params = daily_market_query_params(self.config.symbol.id.sid, window_start, window_end);
        let symbols_by_sid = std::collections::HashMap::from([(
            self.config.symbol.id.sid,
            self.config.symbol.clone(),
        )]);
        if self.config.resolution.is_tick() {
            let grouped = self
                .context
                .store
                .scan_tick_partitions_grouped(&symbols_by_sid, &params)
                .await?;
            let available = grouped
                .get(&self.config.symbol.id.sid)
                .map(|rows| rows.iter().map(|row| row.time.date_utc()).collect())
                .unwrap_or_default();
            Ok(expected.is_subset(&available))
        } else if self.config.tick_type == TickType::Quote {
            let grouped = self
                .context
                .store
                .scan_quote_bar_partitions_grouped(
                    &symbols_by_sid,
                    self.config.resolution,
                    self.config.tick_type,
                    &params,
                )
                .await?;
            let available = grouped
                .get(&self.config.symbol.id.sid)
                .map(|rows| {
                    if self.config.resolution == Resolution::Daily {
                        daily_quote_bar_dates(
                            rows,
                            &expected,
                            &self
                                .context
                                .market_hours_database
                                .exchange_hours(&self.config.symbol),
                        )
                    } else {
                        rows.iter().map(|row| row.end_time.date_utc()).collect()
                    }
                })
                .unwrap_or_default();
            Ok(expected.is_subset(&available))
        } else {
            let grouped = self
                .context
                .store
                .scan_trade_bar_partitions_grouped(
                    &symbols_by_sid,
                    self.config.resolution,
                    self.config.tick_type,
                    &params,
                )
                .await?;
            let rows = grouped
                .get(&self.config.symbol.id.sid)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if rows.is_empty() {
                return Ok(false);
            }
            let exchange_hours = self
                .context
                .market_hours_database
                .exchange_hours(&self.config.symbol);
            let available = daily_trade_bar_dates(rows, &expected, &exchange_hours);
            if expected.is_subset(&available) {
                return Ok(true);
            }
            Ok(rows.iter().any(|row| {
                row.time.date_utc() <= window_end && row.end_time.date_utc() >= window_start
            }))
        }
    }

    async fn load_trade_bars(
        &mut self,
        params: &QueryParams,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let symbols_by_sid = std::collections::HashMap::from([(
            self.config.symbol.id.sid,
            self.config.symbol.clone(),
        )]);
        let grouped = self
            .context
            .store
            .scan_trade_bar_partitions_grouped(
                &symbols_by_sid,
                self.config.resolution,
                self.config.tick_type,
                params,
            )
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        let bars = grouped
            .get(&self.config.symbol.id.sid)
            .cloned()
            .unwrap_or_default();
        self.trade_bars_to_points(bars, window_start, window_end)
    }

    fn trade_bars_to_points(
        &mut self,
        mut bars: Vec<TradeBar>,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let mut out = Vec::with_capacity(bars.len());
        let mut daily_rows: BTreeMap<(u64, i64), (bool, SubscriptionDataPoint)> = BTreeMap::new();
        for bar in bars.drain(..) {
            let mut bar = bar;
            let prefer_row = is_session_close_daily_bar(bar.time, bar.end_time);
            self.normalize_daily_trade_bar_frontier(&mut bar);
            if self.config.resolution == Resolution::Daily
                && !self.trade_bar_belongs_to_window(&bar, window_start, window_end)
            {
                continue;
            }
            normalize_trade_bar(&mut bar, self.config.normalization_mode, &self.factor_rows);
            if !bar.is_valid() {
                continue;
            }
            if !self.is_regular_market_bar(bar.end_time) {
                continue;
            }
            let point = SubscriptionDataPoint::TradeBar(bar);
            if self.config.resolution == Resolution::Daily
                && self.config.symbol.security_type() == SecurityType::Equity
            {
                let key = (self.config.symbol.id.sid, point.frontier_time().0);
                match daily_rows.get(&key) {
                    Some((existing_preferred, _)) if *existing_preferred && !prefer_row => {}
                    _ => {
                        daily_rows.insert(key, (prefer_row, point));
                    }
                }
            } else {
                out.push(point);
            }
        }
        out.extend(daily_rows.into_values().map(|(_, point)| point));
        out.sort_by_key(|point| point.frontier_time().0);
        Ok(out)
    }

    async fn load_quote_bars(
        &mut self,
        params: &QueryParams,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let symbols_by_sid = std::collections::HashMap::from([(
            self.config.symbol.id.sid,
            self.config.symbol.clone(),
        )]);
        let grouped = self
            .context
            .store
            .scan_quote_bar_partitions_grouped(
                &symbols_by_sid,
                self.config.resolution,
                self.config.tick_type,
                params,
            )
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        let bars = grouped
            .get(&self.config.symbol.id.sid)
            .cloned()
            .unwrap_or_default();
        self.quote_bars_to_points(bars, window_start, window_end)
    }

    fn quote_bars_to_points(
        &mut self,
        mut bars: Vec<QuoteBar>,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let mut out = Vec::with_capacity(bars.len());
        let mut daily_rows: BTreeMap<(u64, i64), (bool, SubscriptionDataPoint)> = BTreeMap::new();
        for bar in bars.drain(..) {
            let mut bar = bar;
            let prefer_row = is_session_close_daily_bar(bar.time, bar.end_time);
            self.normalize_daily_quote_bar_frontier(&mut bar);
            if self.config.resolution == Resolution::Daily
                && !self.quote_bar_belongs_to_window(&bar, window_start, window_end)
            {
                continue;
            }
            normalize_quote_bar(&mut bar, self.config.normalization_mode, &self.factor_rows);
            let point = SubscriptionDataPoint::QuoteBar(bar);
            if self.config.resolution == Resolution::Daily
                && self.config.symbol.security_type() == SecurityType::Equity
            {
                let key = (self.config.symbol.id.sid, point.frontier_time().0);
                match daily_rows.get(&key) {
                    Some((existing_preferred, _)) if *existing_preferred && !prefer_row => {}
                    _ => {
                        daily_rows.insert(key, (prefer_row, point));
                    }
                }
            } else {
                out.push(point);
            }
        }
        out.extend(daily_rows.into_values().map(|(_, point)| point));
        out.sort_by_key(|point| point.frontier_time().0);
        Ok(out)
    }

    async fn load_ticks(&self, params: &QueryParams) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let symbols_by_sid = std::collections::HashMap::from([(
            self.config.symbol.id.sid,
            self.config.symbol.clone(),
        )]);
        let grouped = self
            .context
            .store
            .scan_tick_partitions_grouped(&symbols_by_sid, params)
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        let ticks = grouped
            .get(&self.config.symbol.id.sid)
            .cloned()
            .unwrap_or_default();
        Ok(ticks.into_iter().map(SubscriptionDataPoint::Tick).collect())
    }

    async fn load_option_partition(&mut self) -> LeanResult<LoadedPartition> {
        let Some(metadata) = self.config.option_chain.clone() else {
            return Ok(LoadedPartition {
                points: Vec::new(),
                through_date: self.partition_date,
            });
        };
        if self.config.resolution != Resolution::Daily {
            return Ok(LoadedPartition {
                points: Vec::new(),
                through_date: self.partition_date,
            });
        }

        let (window_start, window_end) = self.cache_fill_window();
        let mut points = Vec::new();
        let mut date = window_start;
        while date <= window_end {
            if !self.option_chain_market_is_open(date) {
                date += chrono::Duration::days(1);
                continue;
            }
            let rows = self.option_eod_rows_for_date(&metadata, date).await?;
            if !rows.is_empty() {
                let underlying_price = self
                    .underlying_price_for_option_chain(&metadata, date)
                    .await
                    .unwrap_or(rust_decimal::Decimal::ZERO);
                let frontier_time = equity_daily_frontier(date);
                if let Some(chain) = build_daily_eod_chain(
                    &self.config.symbol,
                    self.config.resolution,
                    metadata.filter,
                    date,
                    rows,
                    underlying_price,
                    frontier_time,
                )
                .map_err(|error| lean_core::LeanError::DataError(error.to_string()))?
                {
                    points.push(SubscriptionDataPoint::OptionChain {
                        canonical_permtick: metadata.canonical_permtick.clone(),
                        chain: Arc::new(chain),
                        frontier_time,
                    });
                }
            }
            date += chrono::Duration::days(1);
        }

        Ok(LoadedPartition {
            points,
            through_date: window_end,
        })
    }

    async fn option_eod_rows_for_date(
        &mut self,
        metadata: &OptionChainSubscriptionMetadata,
        date: chrono::NaiveDate,
    ) -> LeanResult<Vec<OptionEodBar>> {
        let underlyings = vec![metadata.underlying_ticker.to_ascii_uppercase()];
        let mut rows = self
            .context
            .store
            .scan_option_eod_bars(&underlyings, date)
            .await
            .map_err(|error| lean_core::LeanError::DataError(error.to_string()))?;
        if !rows.is_empty() {
            return Ok(rows);
        }

        let Some(provider) = self.context.history_provider.as_ref() else {
            return Ok(Vec::new());
        };
        rows = provider
            .get_option_eod_bars(&metadata.underlying_ticker, date)
            .await
            .map_err(|error| lean_core::LeanError::DataError(error.to_string()))?;
        if !rows.is_empty() {
            self.context
                .store
                .append_option_eod_bars(&rows)
                .await
                .map_err(|error| lean_core::LeanError::DataError(error.to_string()))?;
        }
        Ok(rows)
    }

    async fn underlying_price_for_option_chain(
        &mut self,
        metadata: &OptionChainSubscriptionMetadata,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<rust_decimal::Decimal> {
        let Some(underlying) = self
            .config
            .symbol
            .underlying
            .as_ref()
            .map(|s| (**s).clone())
        else {
            return Ok(rust_decimal::Decimal::ZERO);
        };
        let request = HistoryRequest {
            symbol: underlying.clone(),
            resolution: Resolution::Daily,
            start: partition_day_start(date),
            end: partition_day_end(date),
            data_type: DataType::TradeBar,
        };
        let mut bars = self
            .load_underlying_daily_bars_from_store(&underlying, date)
            .await?;
        if bars.is_empty() {
            if let Some(provider) = self.context.history_provider.as_ref() {
                bars = provider.get_history(&request).await.unwrap_or_default();
                if !bars.is_empty() {
                    self.context
                        .store
                        .append_trade_bars_unchecked(
                            &bars,
                            underlying.security_type(),
                            underlying.market().as_str(),
                            Resolution::Daily,
                            TickType::Trade,
                        )
                        .await?;
                }
            }
        }
        let price = underlying_price_from_bars(&self.config.symbol, &bars);
        if price == rust_decimal::Decimal::ZERO {
            tracing::debug!(
                "missing underlying price for option chain {} on {} ({})",
                metadata.canonical_permtick,
                date,
                option_underlying_ticker(&self.config.symbol)
            );
        }
        Ok(price)
    }

    async fn load_underlying_daily_bars_from_store(
        &self,
        underlying: &Symbol,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<TradeBar>> {
        let params = daily_market_query_params(underlying.id.sid, date, date);
        let symbols_by_sid = HashMap::from([(underlying.id.sid, underlying.clone())]);
        let grouped = self
            .context
            .store
            .scan_trade_bar_partitions_grouped(
                &symbols_by_sid,
                Resolution::Daily,
                TickType::Trade,
                &params,
            )
            .await?;
        Ok(grouped.get(&underlying.id.sid).cloned().unwrap_or_default())
    }

    fn option_chain_market_is_open(&self, date: chrono::NaiveDate) -> bool {
        let Some(underlying) = self.config.symbol.underlying.as_ref() else {
            return true;
        };
        let Some(source_midday) = date.and_hms_opt(12, 0, 0) else {
            return true;
        };
        self.context
            .market_hours_database
            .exchange_hours(underlying.as_ref())
            .is_open_at_local_naive(source_midday)
    }

    async fn load_custom_partition(&self) -> LeanResult<LoadedPartition> {
        let Some(custom) = self.config.custom.as_ref() else {
            return Ok(LoadedPartition {
                points: Vec::new(),
                through_date: self.partition_date,
            });
        };
        let source_type = custom.source_type.clone();
        let ticker = custom.ticker.clone();
        let (window_start, window_end) = self.cache_fill_window();
        let mut points = self
            .context
            .store
            .scan_custom_points_range(&source_type, &ticker, window_start, window_end)
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        let through_date = window_end;
        if points.is_empty() && self.context.options.fetch_missing_custom_data {
            let dataset_is_cached = self
                .context
                .store
                .has_custom_points_dataset(&source_type, &ticker)
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            if !dataset_is_cached {
                if let Err(error) = self.fetch_custom_partition_if_missing().await {
                    tracing::warn!(
                        "failed to fetch custom data partition {}:{} on {}: {:#}",
                        source_type,
                        ticker,
                        self.partition_date,
                        error
                    );
                }
                if points.is_empty() {
                    if let Err(error) = self.fetch_custom_full_history_if_missing().await {
                        tracing::warn!(
                            "failed to fetch custom history {}:{} on {}..{}: {:#}",
                            source_type,
                            ticker,
                            window_start,
                            window_end,
                            error
                        );
                    }
                }
                points = self
                    .context
                    .store
                    .scan_custom_points_range(&source_type, &ticker, window_start, window_end)
                    .await
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            }
        }

        let mut out = Vec::new();
        for point in points {
            let data_point = SubscriptionDataPoint::CustomData {
                symbol: self.config.symbol.clone(),
                ticker: ticker.clone(),
                point,
            };
            if data_point.frontier_time() >= self.start && data_point.frontier_time() <= self.end {
                out.push(data_point);
            }
        }
        Ok(LoadedPartition {
            points: out,
            through_date,
        })
    }

    async fn fetch_custom_partition_if_missing(&self) -> anyhow::Result<()> {
        let Some(custom) = self.config.custom.as_ref() else {
            return Ok(());
        };
        let Some(source) = self
            .context
            .custom_data_sources
            .iter()
            .find(|source| source.name().eq_ignore_ascii_case(&custom.source_type))
        else {
            return Ok(());
        };
        let Some(data_source) =
            source.get_source(&custom.ticker, self.partition_date, &custom.config)
        else {
            return Ok(());
        };
        if self
            .context
            .failed_custom_data_uris
            .lock()
            .expect("custom data failure cache poisoned")
            .contains(&data_source.uri)
        {
            return Ok(());
        }
        let text = match fetch_custom_data_source(&data_source).await {
            Ok(text) => text,
            Err(error) => {
                self.context
                    .failed_custom_data_uris
                    .lock()
                    .expect("custom data failure cache poisoned")
                    .insert(data_source.uri.clone());
                return Err(error).with_context(|| {
                    format!(
                        "failed to fetch custom data source {}:{} on {}",
                        custom.source_type, custom.ticker, self.partition_date
                    )
                });
            }
        };
        let points = text
            .lines()
            .filter_map(|line| source.reader(line, self.partition_date, &custom.config))
            .collect::<Vec<_>>();
        self.context
            .store
            .append_custom_points(&custom.source_type, &custom.ticker, &points)
            .await
            .with_context(|| {
                format!(
                    "failed to persist custom data rows for {}:{} on {}",
                    custom.source_type, custom.ticker, self.partition_date
                )
            })?;
        Ok(())
    }

    async fn fetch_custom_full_history_if_missing(&self) -> anyhow::Result<()> {
        let Some(custom) = self.config.custom.as_ref() else {
            return Ok(());
        };
        let Some(source) = self
            .context
            .custom_data_sources
            .iter()
            .find(|source| source.name().eq_ignore_ascii_case(&custom.source_type))
            .cloned()
        else {
            return Ok(());
        };
        if !source.is_full_history_source() {
            return Ok(());
        }
        let mut history_config = custom.config.clone();
        history_config.query = custom.config.query.merge(&CustomDataQuery {
            start_date: Some(self.start.date_utc()),
            end_date: Some(self.end.date_utc()),
            start_time: Some(self.start),
            end_time: Some(self.end),
            ..custom.dynamic_query.clone()
        });
        let ticker = custom.ticker.clone();
        let source_for_history = source.clone();
        let history_result = tokio::task::spawn_blocking(move || {
            source_for_history.history(&ticker, &history_config)
        })
        .await
        .context("custom history worker join failed")?;
        let points = if let Some(result) = history_result {
            result.map_err(|error| anyhow::anyhow!(error))?
        } else {
            let Some(fallback_source) = self.custom_data_source(&custom.source_type) else {
                return Ok(());
            };
            let Some(data_source) =
                fallback_source.get_source(&custom.ticker, self.partition_date, &custom.config)
            else {
                return Ok(());
            };
            if self
                .context
                .failed_custom_data_uris
                .lock()
                .expect("custom data failure cache poisoned")
                .contains(&data_source.uri)
            {
                return Ok(());
            }
            let text = match fetch_custom_data_source(&data_source).await {
                Ok(text) => text,
                Err(error) => {
                    self.context
                        .failed_custom_data_uris
                        .lock()
                        .expect("custom data failure cache poisoned")
                        .insert(data_source.uri.clone());
                    return Err(error).with_context(|| {
                        format!(
                            "failed to fetch full custom history {}:{}",
                            custom.source_type, custom.ticker
                        )
                    });
                }
            };
            text.lines()
                .filter_map(|line| fallback_source.read_history_line(line, &custom.config))
                .collect::<Vec<_>>()
        };
        self.context
            .store
            .append_custom_points(&custom.source_type, &custom.ticker, &points)
            .await
            .with_context(|| {
                format!(
                    "failed to persist custom history rows for {}:{}",
                    custom.source_type, custom.ticker
                )
            })?;
        Ok(())
    }

    fn custom_data_source(&self, source_type: &str) -> Option<&dyn ICustomDataSource> {
        self.context
            .custom_data_sources
            .iter()
            .find(|source| source.name().eq_ignore_ascii_case(source_type))
            .map(|source| source.as_ref())
    }

    fn is_regular_market_bar(&self, end_time: DateTime) -> bool {
        if self.config.resolution == lean_core::Resolution::Daily {
            return true;
        }
        if self.config.symbol.security_type() != SecurityType::Equity {
            return true;
        }

        let probe_time = self
            .config
            .resolution
            .to_time_span()
            .map(|period| end_time - period)
            .unwrap_or(end_time);
        self.context
            .market_hours_database
            .exchange_hours(&self.config.symbol)
            .is_open_at(probe_time)
    }

    fn normalize_daily_trade_bar_frontier(&mut self, row: &mut TradeBar) {
        if self.config.resolution != Resolution::Daily
            || self.config.symbol.security_type() != SecurityType::Equity
            || row.time.date_utc() == row.end_time.date_utc()
        {
            return;
        }
        if is_midnight_daily_bar(row.time, row.end_time) {
            row.end_time = equity_daily_frontier(row.time.date_utc());
            row.time = row.end_time - row.period;
            return;
        }
        if is_source_dated_daily_bar(row.time, row.end_time) {
            row.end_time = equity_daily_frontier(row.time.date_utc());
            row.time = row.end_time;
            return;
        }
        if is_session_close_daily_bar(row.time, row.end_time) {
            return;
        }
        if self.daily_time_is_source_date.unwrap_or_else(|| {
            let inferred = is_open_equity_date(
                &self
                    .context
                    .market_hours_database
                    .exchange_hours(&self.config.symbol),
                row.time.date_utc(),
            );
            self.daily_time_is_source_date = Some(inferred);
            inferred
        }) {
            row.end_time = row.time;
            row.time = row.end_time - row.period;
        }
    }

    fn normalize_daily_quote_bar_frontier(&mut self, row: &mut QuoteBar) {
        if self.config.resolution != Resolution::Daily
            || self.config.symbol.security_type() != SecurityType::Equity
            || row.time.date_utc() == row.end_time.date_utc()
        {
            return;
        }
        if is_midnight_daily_bar(row.time, row.end_time) {
            row.end_time = equity_daily_frontier(row.time.date_utc());
            row.time = row.end_time - row.period;
            return;
        }
        if is_source_dated_daily_bar(row.time, row.end_time) {
            row.end_time = equity_daily_frontier(row.time.date_utc());
            row.time = row.end_time;
            return;
        }
        if is_session_close_daily_bar(row.time, row.end_time) {
            return;
        }
        if self.daily_time_is_source_date.unwrap_or_else(|| {
            let inferred = is_open_equity_date(
                &self
                    .context
                    .market_hours_database
                    .exchange_hours(&self.config.symbol),
                row.time.date_utc(),
            );
            self.daily_time_is_source_date = Some(inferred);
            inferred
        }) {
            row.end_time = row.time;
            row.time = row.end_time - row.period;
        }
    }

    fn trade_bar_belongs_to_window(
        &self,
        row: &TradeBar,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> bool {
        if row.symbol != self.config.symbol {
            return false;
        }
        if self.config.resolution == Resolution::Daily {
            timestamp_belongs_to_window(row.time, window_start, window_end)
                || self.daily_row_covers_window(row.time, row.end_time, window_start, window_end)
        } else {
            timestamp_belongs_to_window(
                market_partition_time(row.time, row.end_time),
                window_start,
                window_end,
            )
        }
    }

    fn quote_bar_belongs_to_window(
        &self,
        row: &QuoteBar,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> bool {
        if row.symbol != self.config.symbol {
            return false;
        }
        if self.config.resolution == Resolution::Daily {
            timestamp_belongs_to_window(row.time, window_start, window_end)
                || self.daily_row_covers_window(row.time, row.end_time, window_start, window_end)
        } else {
            timestamp_belongs_to_window(
                market_partition_time(row.time, row.end_time),
                window_start,
                window_end,
            )
        }
    }

    fn daily_row_covers_window(
        &self,
        time: DateTime,
        end_time: DateTime,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> bool {
        let coverage_date = if time.date_utc() != end_time.date_utc() {
            end_time.date_utc()
        } else if time.to_utc().time() == chrono::NaiveTime::MIN
            && end_time.to_utc().time() == chrono::NaiveTime::from_hms_opt(16, 0, 0).unwrap()
        {
            end_time.date_utc()
        } else {
            let exchange_hours = self
                .context
                .market_hours_database
                .exchange_hours(&self.config.symbol);
            daily_row_coverage_date(&exchange_hours, time, end_time)
        };
        coverage_date >= window_start && coverage_date <= window_end
    }

    fn tick_belongs_to_window(
        &self,
        row: &Tick,
        window_start: chrono::NaiveDate,
        window_end: chrono::NaiveDate,
    ) -> bool {
        row.symbol == self.config.symbol
            && timestamp_belongs_to_window(row.time, window_start, window_end)
    }

    fn cache_fill_window(&self) -> (chrono::NaiveDate, chrono::NaiveDate) {
        let intervals = self.context.options.cache_policy.prefetch_intervals.max(1);
        let start = self.partition_date;
        let mut end = match self.config.resolution {
            Resolution::Daily => add_years_saturating(start, intervals as i32),
            Resolution::Hour => add_months_saturating(start, intervals as i32),
            Resolution::Tick | Resolution::Second | Resolution::Minute => {
                start + chrono::Duration::days(intervals.saturating_sub(1) as i64)
            }
        };
        if end > self.end_partition_date {
            end = self.end_partition_date;
        }
        (start, end)
    }

    fn exchange_hours(&self) -> Option<std::sync::Arc<ExchangeHours>> {
        if self.config.symbol.security_type() == SecurityType::Equity {
            Some(
                self.context
                    .market_hours_database
                    .exchange_hours(&self.config.symbol),
            )
        } else {
            None
        }
    }

    fn should_skip_partition_date(&self) -> bool {
        if self.config.data_kind != SubscriptionDataKind::Market
            || self.config.resolution != Resolution::Daily
            || self.config.symbol.security_type() != SecurityType::Equity
        {
            return false;
        }
        let Some(source_midday) = self.partition_date.and_hms_opt(12, 0, 0) else {
            return false;
        };
        !self
            .context
            .market_hours_database
            .exchange_hours(&self.config.symbol)
            .is_open_at_local_naive(source_midday)
    }
}

async fn fetch_custom_data_source(source: &CustomDataSource) -> anyhow::Result<String> {
    match source.transport {
        CustomDataTransport::LocalFile => tokio::fs::read_to_string(&source.uri)
            .await
            .with_context(|| format!("failed to read custom data file {}", source.uri)),
        CustomDataTransport::Http => {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .context("failed to build custom data HTTP client")?;
            let mut last_error = None;
            for attempt in 1..=3 {
                let result = async {
                    client
                        .get(&source.uri)
                        .send()
                        .await
                        .with_context(|| {
                            format!("failed to request custom data url {}", source.uri)
                        })?
                        .error_for_status()
                        .with_context(|| {
                            format!("custom data url returned an error {}", source.uri)
                        })?
                        .text()
                        .await
                        .with_context(|| {
                            format!("failed to read custom data response {}", source.uri)
                        })
                }
                .await;

                match result {
                    Ok(text) => return Ok(text),
                    Err(error) if attempt < 3 => {
                        tracing::warn!(
                            "custom data fetch attempt {} failed for {}: {:#}",
                            attempt,
                            source.uri,
                            error
                        );
                        last_error = Some(error);
                        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64))
                            .await;
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow::anyhow!("custom data fetch did not run")))
        }
    }
}

fn partition_day_start(date: chrono::NaiveDate) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()))
}

fn partition_day_end(date: chrono::NaiveDate) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(23, 59, 59).unwrap()))
}

fn daily_market_query_params(
    sid: u64,
    window_start: chrono::NaiveDate,
    window_end: chrono::NaiveDate,
) -> QueryParams {
    let query_start = window_start.pred_opt().unwrap_or(window_start);
    let query_end = window_end.succ_opt().unwrap_or(window_end);
    QueryParams::new()
        .with_day_range(
            partition_day_start(query_start),
            partition_day_end(query_end),
        )
        .with_symbols(vec![sid])
}

fn timestamp_belongs_to_window(
    timestamp: DateTime,
    window_start: chrono::NaiveDate,
    window_end: chrono::NaiveDate,
) -> bool {
    timestamp >= partition_day_start(window_start) && timestamp <= partition_day_end(window_end)
}

fn daily_point_coverage_date(
    point: &SubscriptionDataPoint,
    _exchange_hours: &ExchangeHours,
) -> Option<chrono::NaiveDate> {
    match point {
        SubscriptionDataPoint::TradeBar(_) | SubscriptionDataPoint::QuoteBar(_) => {
            Some(point.frontier_time().date_utc())
        }
        _ => None,
    }
}

fn is_open_equity_date(exchange_hours: &ExchangeHours, date: chrono::NaiveDate) -> bool {
    date.and_hms_opt(12, 0, 0)
        .map(|midday| exchange_hours.is_open_at_local_naive(midday))
        .unwrap_or(false)
}

fn add_years_saturating(date: chrono::NaiveDate, years: i32) -> chrono::NaiveDate {
    date.with_year(date.year().saturating_add(years))
        .unwrap_or_else(|| {
            chrono::NaiveDate::from_ymd_opt(date.year().saturating_add(years), 2, 28)
                .unwrap_or(date)
        })
        - chrono::Duration::days(1)
}

fn add_months_saturating(date: chrono::NaiveDate, months: i32) -> chrono::NaiveDate {
    let month0 = date.month0() as i32 + months;
    let year = date.year().saturating_add(month0.div_euclid(12));
    let month0 = month0.rem_euclid(12) as u32;
    let month = month0 + 1;
    let day = date.day().min(days_in_month(year, month));
    chrono::NaiveDate::from_ymd_opt(year, month, day).unwrap_or(date) - chrono::Duration::days(1)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (next_year, next_month) = if month == 12 {
        (year.saturating_add(1), 1)
    } else {
        (year, month + 1)
    };
    chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1)
        .and_then(|date| date.pred_opt())
        .map(|date| date.day())
        .unwrap_or(28)
}

fn expected_market_dates(
    window_start: chrono::NaiveDate,
    window_end: chrono::NaiveDate,
    hard_end: chrono::NaiveDate,
    exchange_hours: Option<&ExchangeHours>,
    security_type: SecurityType,
) -> HashSet<chrono::NaiveDate> {
    let end = window_end.min(hard_end);
    let mut out = HashSet::new();
    let mut date = window_start;
    while date <= end {
        let include = if security_type == SecurityType::Equity {
            date.and_hms_opt(12, 0, 0)
                .and_then(|midday| exchange_hours.map(|hours| hours.is_open_at_local_naive(midday)))
                .unwrap_or(false)
        } else {
            true
        };
        if include {
            out.insert(date);
        }
        date += chrono::Duration::days(1);
    }
    out
}

fn days_since_epoch(ns: i64) -> i32 {
    let secs = ns.div_euclid(1_000_000_000);
    let days = secs.div_euclid(86_400);
    days as i32
}

fn date_from_days_since_epoch(days: i32) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|epoch| epoch.checked_add_signed(chrono::Duration::days(days as i64)))
}

fn daily_trade_bar_dates(
    rows: &[TradeBar],
    expected: &HashSet<chrono::NaiveDate>,
    exchange_hours: &ExchangeHours,
) -> HashSet<chrono::NaiveDate> {
    let mut dates = HashSet::new();
    let mut unique_rows = HashSet::new();
    for row in rows {
        let key = (row.symbol.id.sid, row.time.0, row.end_time.0);
        let date = daily_row_coverage_date(exchange_hours, row.time, row.end_time);
        if expected.contains(&date) {
            dates.insert(date);
            unique_rows.insert(key);
        }
    }
    if unique_rows.len() < expected.len() {
        dates.clear();
    }
    dates
}

fn daily_quote_bar_dates(
    rows: &[QuoteBar],
    expected: &HashSet<chrono::NaiveDate>,
    exchange_hours: &ExchangeHours,
) -> HashSet<chrono::NaiveDate> {
    let mut dates = HashSet::new();
    let mut unique_rows = HashSet::new();
    for row in rows {
        let key = (row.symbol.id.sid, row.time.0, row.end_time.0);
        let date = daily_row_coverage_date(exchange_hours, row.time, row.end_time);
        if expected.contains(&date) {
            dates.insert(date);
            unique_rows.insert(key);
        }
    }
    if unique_rows.len() < expected.len() {
        dates.clear();
    }
    dates
}

fn fetched_daily_trade_bar_dates(
    rows: &[TradeBar],
    window_start: chrono::NaiveDate,
    expected: &HashSet<chrono::NaiveDate>,
    exchange_hours: &ExchangeHours,
) -> HashSet<chrono::NaiveDate> {
    rows.iter()
        .filter_map(|row| {
            let date = fetched_daily_row_coverage_date(
                exchange_hours,
                row.time,
                row.end_time,
                window_start,
            );
            expected.contains(&date).then_some(date)
        })
        .collect()
}

fn fetched_daily_quote_bar_dates(
    rows: &[QuoteBar],
    window_start: chrono::NaiveDate,
    expected: &HashSet<chrono::NaiveDate>,
    exchange_hours: &ExchangeHours,
) -> HashSet<chrono::NaiveDate> {
    rows.iter()
        .filter_map(|row| {
            let date = fetched_daily_row_coverage_date(
                exchange_hours,
                row.time,
                row.end_time,
                window_start,
            );
            expected.contains(&date).then_some(date)
        })
        .collect()
}

fn daily_row_coverage_date(
    exchange_hours: &ExchangeHours,
    time: DateTime,
    end_time: DateTime,
) -> chrono::NaiveDate {
    if is_midnight_daily_bar(time, end_time) {
        return time.date_utc();
    }
    if is_source_dated_daily_bar(time, end_time) {
        return time.date_utc();
    }
    let time_date = time.date_utc();
    if time_date != end_time.date_utc() {
        return end_time.date_utc();
    }
    if is_open_equity_date(exchange_hours, time_date) {
        time_date
    } else {
        end_time.date_utc()
    }
}

fn fetched_daily_row_coverage_date(
    exchange_hours: &ExchangeHours,
    time: DateTime,
    end_time: DateTime,
    window_start: chrono::NaiveDate,
) -> chrono::NaiveDate {
    if is_midnight_daily_bar(time, end_time) {
        return time.date_utc().max(window_start);
    }
    if time.date_utc() == end_time.date_utc() && time.to_utc().time() == chrono::NaiveTime::MIN {
        return time.date_utc().max(window_start);
    }
    if is_session_close_daily_bar(time, end_time) {
        return market_partition_time(time, end_time)
            .date_utc()
            .max(window_start);
    }
    daily_row_coverage_date(exchange_hours, time, end_time)
        .max(window_start)
        .min(end_time.date_utc())
}

fn contiguous_covered_until(
    window_start: chrono::NaiveDate,
    window_end: chrono::NaiveDate,
    hard_end: chrono::NaiveDate,
    expected: &HashSet<chrono::NaiveDate>,
    covered: &HashSet<chrono::NaiveDate>,
) -> Option<chrono::NaiveDate> {
    let mut date = window_start;
    let end = window_end.min(hard_end);
    let mut last = None;
    while date <= end {
        if expected.contains(&date) && !covered.contains(&date) {
            break;
        }
        last = Some(date);
        date += chrono::Duration::days(1);
    }
    last
}

fn market_partition_time(time: DateTime, end_time: DateTime) -> DateTime {
    if time.date_utc() != end_time.date_utc() {
        end_time
    } else {
        time
    }
}

fn is_midnight_daily_bar(time: DateTime, end_time: DateTime) -> bool {
    time.to_utc().time() == chrono::NaiveTime::MIN
        && end_time.to_utc().time() == chrono::NaiveTime::MIN
        && time.date_utc() != end_time.date_utc()
}

fn is_source_dated_daily_bar(time: DateTime, end_time: DateTime) -> bool {
    time.date_utc() != end_time.date_utc()
        && time.to_utc().time() == end_time.to_utc().time()
        && time.to_utc().time() == chrono::NaiveTime::from_hms_opt(16, 0, 0).unwrap()
}

fn is_session_close_daily_bar(time: DateTime, end_time: DateTime) -> bool {
    !is_midnight_daily_bar(time, end_time) && !is_source_dated_daily_bar(time, end_time)
}

fn equity_daily_frontier(date: chrono::NaiveDate) -> DateTime {
    DateTime::from(date.and_hms_opt(16, 0, 0).expect("valid equity close time"))
}

fn fill_forward_point(
    last_point: &SubscriptionDataPoint,
    fill_frontier: DateTime,
    period: lean_core::TimeSpan,
) -> Option<SubscriptionDataPoint> {
    match last_point {
        SubscriptionDataPoint::TradeBar(bar) => {
            let mut fill = bar.clone();
            fill.end_time = fill_frontier;
            fill.time = fill_frontier - period;
            fill.open = fill.close;
            fill.high = fill.close;
            fill.low = fill.close;
            fill.volume = rust_decimal_macros::dec!(0);
            fill.period = period;
            Some(SubscriptionDataPoint::TradeBar(fill))
        }
        SubscriptionDataPoint::QuoteBar(bar) => {
            let mut fill = bar.clone();
            fill.end_time = fill_frontier;
            fill.time = fill_frontier - period;
            fill.period = period;
            Some(SubscriptionDataPoint::QuoteBar(fill))
        }
        SubscriptionDataPoint::Tick(_)
        | SubscriptionDataPoint::CustomData { .. }
        | SubscriptionDataPoint::OptionChain { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_feed::{DataFeedContext, DataFeedOptions, SubscriptionCachePolicy};
    use async_trait::async_trait;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{DataNormalizationMode, Market, Resolution, Symbol, TimeSpan};
    use lean_data::{
        CustomDataConfig, CustomDataPoint, CustomDataQuery, CustomSubscriptionMetadata,
        SubscriptionDataConfig, TradeBar, TradeBarData,
    };
    use lean_data_providers::{CustomDataContext, HistoryRequest, IHistoryProvider};
    use lean_storage::IcebergStore;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };

    fn dt(date: NaiveDate, hour: u32, minute: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, 0).unwrap()))
    }

    fn trade_bar(symbol: Symbol, date: NaiveDate, hour: u32, minute: u32, close: i64) -> TradeBar {
        TradeBar::new(
            symbol,
            dt(date, hour, minute),
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), Decimal::from(close), dec!(100)),
        )
    }

    fn daily_trade_bar_start_dated(symbol: Symbol, date: NaiveDate, close: i64) -> TradeBar {
        let price = Decimal::from(close);
        TradeBar::new(
            symbol,
            dt(date, 16, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(price, price, price, price, dec!(100)),
        )
    }

    fn context(store: Arc<IcebergStore>) -> DataFeedContext {
        DataFeedContext::new(store).with_options(DataFeedOptions {
            cache_policy: SubscriptionCachePolicy {
                prefetch_intervals: 1,
                max_prefetch_rows: 16,
            },
            fetch_missing_custom_data: true,
        })
    }

    struct FixtureHistorySource;

    impl ICustomDataSource for FixtureHistorySource {
        fn initialize(&mut self, _context: &CustomDataContext) {}

        fn name(&self) -> &str {
            "fixture"
        }

        fn is_full_history_source(&self) -> bool {
            true
        }

        fn history(
            &self,
            _ticker: &str,
            _config: &CustomDataConfig,
        ) -> Option<Result<Vec<CustomDataPoint>, String>> {
            let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
            Some(Ok(vec![CustomDataPoint {
                time: day,
                end_time: Some(dt(day, 16, 0)),
                value: dec!(42),
                fields: std::collections::HashMap::new(),
            }]))
        }
    }

    struct ErrorHistoryProvider;

    #[async_trait]
    impl IHistoryProvider for ErrorHistoryProvider {
        async fn get_history(&self, _request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
            anyhow::bail!("provider exploded")
        }
    }

    struct OneBarProvider {
        bar: TradeBar,
    }

    #[async_trait]
    impl IHistoryProvider for OneBarProvider {
        async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
            if self.bar.symbol.id.sid == request.symbol.id.sid {
                Ok(vec![self.bar.clone()])
            } else {
                Ok(Vec::new())
            }
        }
    }

    struct SlowConcurrentHistoryProvider {
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    impl SlowConcurrentHistoryProvider {
        fn new() -> Self {
            Self {
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
            }
        }

        fn max_active(&self) -> usize {
            self.max_active.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl IHistoryProvider for SlowConcurrentHistoryProvider {
        async fn get_history(&self, request: &HistoryRequest) -> anyhow::Result<Vec<TradeBar>> {
            let now_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now_active, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            let day = request.start.date_utc();
            Ok(vec![TradeBar::new(
                request.symbol.clone(),
                dt(day, 16, 0),
                TimeSpan::ONE_DAY,
                TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
            )])
        }
    }

    #[test]
    fn cache_fill_window_policy_uses_existing_prefetch_intervals() {
        let daily_start = NaiveDate::from_ymd_opt(2022, 1, 3).unwrap();
        assert_eq!(
            add_years_saturating(daily_start, 1),
            NaiveDate::from_ymd_opt(2023, 1, 2).unwrap()
        );

        let hourly_start = NaiveDate::from_ymd_opt(2022, 1, 31).unwrap();
        assert_eq!(
            add_months_saturating(hourly_start, 1),
            NaiveDate::from_ymd_opt(2022, 2, 27).unwrap()
        );
    }

    #[test]
    fn expected_market_dates_skips_closed_equity_days() {
        let start = NaiveDate::from_ymd_opt(2022, 1, 1).unwrap();
        let end = NaiveDate::from_ymd_opt(2022, 1, 5).unwrap();
        let hours = ExchangeHours::us_equity();
        let dates = expected_market_dates(start, end, end, Some(&hours), SecurityType::Equity);

        assert!(!dates.contains(&NaiveDate::from_ymd_opt(2022, 1, 1).unwrap()));
        assert!(!dates.contains(&NaiveDate::from_ymd_opt(2022, 1, 2).unwrap()));
        assert!(dates.contains(&NaiveDate::from_ymd_opt(2022, 1, 3).unwrap()));
    }

    #[tokio::test]
    async fn prefetch_does_not_make_future_frontier_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let bars = [
            trade_bar(symbol.clone(), day, 15, 0, 10),
            trade_bar(symbol.clone(), day, 15, 1, 11),
        ];

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let mut stream =
            SubscriptionProducerState::new(config, context(store), dt(day, 0, 0), dt(day, 23, 59));

        stream.stage_loaded_points(
            bars.iter()
                .cloned()
                .map(SubscriptionDataPoint::TradeBar)
                .collect(),
        );
        stream.promote_next_frontier();
        assert_eq!(
            stream.pending.front().unwrap().frontier_time(),
            bars[0].end_time,
            "only the first frontier should be algorithm-visible"
        );
        assert!(
            stream.prefetched.contains_key(&bars[1].end_time),
            "the next interval may be cached internally"
        );

        let first = stream.pending.pop_front().unwrap().point;
        stream.last_emitted_time = Some(first.frontier_time());
        stream.last_emitted_point = Some(first.clone());
        assert_eq!(first.frontier_time(), bars[0].end_time);
        stream.promote_next_frontier();
        assert_eq!(
            stream.pending.front().unwrap().frontier_time(),
            bars[1].end_time
        );
    }

    #[tokio::test]
    async fn intraday_local_market_window_uses_partition_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let day3 = NaiveDate::from_ymd_opt(2024, 1, 18).unwrap();

        store
            .append_trade_bars(
                &[
                    trade_bar(symbol.clone(), day1, 15, 31, 10),
                    trade_bar(symbol.clone(), day2, 15, 31, 11),
                ],
                SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Minute,
                TickType::Trade,
            )
            .await
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let stream = SubscriptionProducerState::new(
            config,
            context(store),
            dt(day1, 0, 0),
            dt(day3, 23, 59),
        );

        assert!(stream.has_local_market_window(day1, day2).await.unwrap());
        assert!(!stream.has_local_market_window(day1, day3).await.unwrap());
    }

    #[tokio::test]
    async fn partial_daily_local_market_window_skips_remote_refetch() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let day4 = NaiveDate::from_ymd_opt(2024, 1, 5).unwrap();

        store
            .append_trade_bars(
                &[
                    trade_bar(symbol.clone(), day1, 16, 0, 10),
                    trade_bar(symbol.clone(), day4, 16, 0, 11),
                ],
                SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let stream = SubscriptionProducerState::new(
            config,
            context(store),
            dt(day1, 0, 0),
            dt(day4, 23, 59),
        );

        assert!(
            stream.has_local_market_window(day1, day4).await.unwrap(),
            "partial cached daily history should satisfy the local window check"
        );
    }

    #[tokio::test]
    async fn fill_forward_stages_missing_non_tick_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let first = trade_bar(symbol.clone(), day, 15, 0, 10);
        let next_real = trade_bar(symbol.clone(), day, 15, 2, 12);

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        let mut stream =
            SubscriptionProducerState::new(config, context(store), dt(day, 0, 0), dt(day, 23, 59));

        stream.stage_loaded_points(vec![SubscriptionDataPoint::TradeBar(first.clone())]);
        stream.promote_next_frontier();
        let emitted = stream.pending.pop_front().unwrap().point;
        stream.last_emitted_time = Some(emitted.frontier_time());
        stream.last_emitted_point = Some(emitted.clone());
        assert_eq!(emitted.frontier_time(), first.end_time);

        stream.maybe_stage_fill_forward(next_real.end_time);
        stream.stage_loaded_points(vec![SubscriptionDataPoint::TradeBar(next_real.clone())]);
        stream.promote_next_frontier();

        let fill = stream.pending.pop_front().unwrap().point;
        assert_eq!(fill.frontier_time(), first.end_time + TimeSpan::ONE_MINUTE);
        match fill {
            SubscriptionDataPoint::TradeBar(bar) => {
                assert_eq!(bar.close, first.close);
                assert_eq!(bar.volume, dec!(0));
            }
            _ => panic!("expected fill-forward trade bar"),
        }

        stream.promote_next_frontier();
        assert_eq!(
            stream.pending.front().unwrap().frontier_time(),
            next_real.end_time
        );
    }

    #[tokio::test]
    async fn daily_stream_loads_start_dated_cache_rows_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let bar = daily_trade_bar_start_dated(symbol.clone(), day, 10);
        store
            .append_trade_bars(
                std::slice::from_ref(&bar),
                SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let mut stream = SubscriptionStream::new(
            config,
            context(store),
            dt(day, 0, 0),
            dt(day + chrono::Duration::days(1), 23, 59),
        );

        let first = stream.pop_next().await.unwrap().expect("daily bar");
        assert_eq!(first.frontier_time(), bar.time);
        assert!(stream.pop_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn daily_stream_does_not_emit_previous_source_dated_row_for_next_day() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let previous_day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let next_day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let bar = daily_trade_bar_start_dated(symbol.clone(), previous_day, 10);
        store
            .append_trade_bars(
                std::slice::from_ref(&bar),
                SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let mut stream = SubscriptionStream::new(
            config,
            context(store),
            dt(next_day, 0, 0),
            dt(next_day, 23, 59),
        );

        assert!(
            stream.pop_next().await.unwrap().is_none(),
            "a source-dated daily row belongs to its time date, not the next day in end_time"
        );
    }

    #[tokio::test]
    async fn daily_stream_batches_cached_window_and_advances_partition() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let first_day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let second_day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        let bars = [
            daily_trade_bar_start_dated(symbol.clone(), first_day, 10),
            daily_trade_bar_start_dated(symbol.clone(), second_day, 11),
        ];
        store
            .append_trade_bars(
                &bars,
                SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let mut stream = SubscriptionStream::new(
            config,
            context(store),
            dt(first_day, 0, 0),
            dt(second_day, 23, 59),
        );

        let first = stream.pop_next().await.unwrap().expect("first daily bar");
        assert_eq!(first.frontier_time(), dt(first_day, 16, 0));
        let second = stream.pop_next().await.unwrap().expect("second daily bar");
        assert_eq!(second.frontier_time(), dt(second_day, 16, 0));
        assert!(stream.pop_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn custom_stream_batches_cached_window_and_advances_partition() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_base("fixture", "ALT", &Market::usa());
        let first_day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let second_day = NaiveDate::from_ymd_opt(2024, 1, 17).unwrap();
        store
            .append_custom_points(
                "fixture",
                "ALT",
                &[
                    CustomDataPoint {
                        time: first_day,
                        end_time: Some(dt(first_day, 16, 0)),
                        value: dec!(10),
                        fields: std::collections::HashMap::new(),
                    },
                    CustomDataPoint {
                        time: second_day,
                        end_time: Some(dt(second_day, 16, 0)),
                        value: dec!(11),
                        fields: std::collections::HashMap::new(),
                    },
                ],
            )
            .await
            .unwrap();

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
        let config = SubscriptionDataConfig::new_custom(symbol, Resolution::Daily, metadata);
        let mut stream = SubscriptionStream::new(
            config,
            context(store),
            dt(first_day, 0, 0),
            dt(second_day, 23, 59),
        );

        let first = stream
            .pop_next()
            .await
            .unwrap()
            .expect("first custom point");
        assert_eq!(first.frontier_time(), dt(first_day, 16, 0));
        let second = stream
            .pop_next()
            .await
            .unwrap()
            .expect("second custom point");
        assert_eq!(second.frontier_time(), dt(second_day, 16, 0));
        assert!(stream.pop_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn custom_stream_fetches_provider_history_and_persists_to_iceberg_on_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_base("fixture", "ALT", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
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
        let config = SubscriptionDataConfig::new_custom(symbol, Resolution::Daily, metadata);
        let context =
            context(store.clone()).with_custom_data_sources(vec![Arc::new(FixtureHistorySource)]);
        let mut stream = SubscriptionStream::new(config, context, dt(day, 0, 0), dt(day, 23, 59));

        let first = stream.pop_next().await.unwrap().expect("custom point");
        assert_eq!(first.frontier_time(), dt(day, 16, 0));

        let persisted = store
            .scan_custom_points("fixture", "ALT", day)
            .await
            .unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].value, dec!(42));
    }

    #[tokio::test]
    async fn custom_full_history_source_emits_through_background_channel_once() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_base("fixture", "ALT", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
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
        let config = SubscriptionDataConfig::new_custom(symbol, Resolution::Daily, metadata);
        let context =
            context(store.clone()).with_custom_data_sources(vec![Arc::new(FixtureHistorySource)]);
        let mut stream = SubscriptionStream::new(config, context, dt(day, 0, 0), dt(day, 23, 59));

        let first = stream.pop_next().await.unwrap().expect("custom point");
        assert_eq!(first.frontier_time(), dt(day, 16, 0));
        assert!(stream.pop_next().await.unwrap().is_none());
        let persisted = store
            .scan_custom_points("fixture", "ALT", day)
            .await
            .unwrap();
        assert_eq!(persisted.len(), 1);
    }

    #[tokio::test]
    async fn provider_fetch_errors_remain_nonfatal_cache_misses() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let config = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let context = context(store).with_history_provider(Some(Arc::new(ErrorHistoryProvider)));
        let mut stream = SubscriptionStream::new(config, context, dt(day, 0, 0), dt(day, 23, 59));

        assert!(
            stream.pop_next().await.unwrap().is_none(),
            "provider fetch errors are warnings so missing local data remains an empty stream"
        );
    }

    #[tokio::test]
    async fn provider_midnight_daily_bar_emits_on_source_session_frontier() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let day = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let bar = TradeBar::new(
            symbol.clone(),
            dt(day, 0, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(100)),
        );
        let config = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let context = context(store).with_history_provider(Some(Arc::new(OneBarProvider { bar })));
        let mut stream = SubscriptionStream::new(config, context, dt(day, 0, 0), dt(day, 23, 59));

        let point = stream.pop_next().await.unwrap().expect("daily bar");
        assert_eq!(point.frontier_time(), dt(day, 16, 0));
    }

    #[tokio::test]
    async fn cache_miss_producers_fetch_different_subscriptions_concurrently() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let provider = Arc::new(SlowConcurrentHistoryProvider::new());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let qqq = Symbol::create_equity("QQQ", &Market::usa());
        let context = context(store).with_history_provider(Some(provider.clone()));
        let mut spy_stream = SubscriptionStream::new(
            SubscriptionDataConfig::new_equity(spy, Resolution::Daily, DataNormalizationMode::Raw),
            context.clone(),
            dt(day, 0, 0),
            dt(day, 23, 59),
        );
        let mut qqq_stream = SubscriptionStream::new(
            SubscriptionDataConfig::new_equity(qqq, Resolution::Daily, DataNormalizationMode::Raw),
            context,
            dt(day, 0, 0),
            dt(day, 23, 59),
        );

        let (spy_point, qqq_point) = tokio::join!(spy_stream.pop_next(), qqq_stream.pop_next());
        assert!(spy_point.unwrap().is_some());
        assert!(qqq_point.unwrap().is_some());
        assert!(
            provider.max_active() >= 2,
            "background producers should overlap provider fetches"
        );
    }

    #[tokio::test]
    async fn dropping_subscription_stream_aborts_background_producer() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let provider = Arc::new(SlowConcurrentHistoryProvider::new());
        let day = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let finished = Arc::new(AtomicBool::new(false));
        let finished_probe = finished.clone();
        {
            let context = context(store).with_history_provider(Some(provider));
            let stream = SubscriptionStream::new(
                SubscriptionDataConfig::new_equity(
                    symbol,
                    Resolution::Daily,
                    DataNormalizationMode::Raw,
                ),
                context,
                dt(day, 0, 0),
                dt(day, 23, 59),
            );
            let producer = stream.producer.abort_handle();
            drop(stream);
            tokio::spawn(async move {
                while !producer.is_finished() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                finished_probe.store(true, Ordering::SeqCst);
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(finished.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn daily_stream_moves_closed_end_date_to_source_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let friday = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let bar = daily_trade_bar_start_dated(symbol.clone(), friday, 10);
        assert_eq!(bar.end_time.date_utc(), friday + chrono::Duration::days(1));
        store
            .append_trade_bars(
                std::slice::from_ref(&bar),
                SecurityType::Equity,
                Market::usa().as_str(),
                Resolution::Daily,
                TickType::Trade,
            )
            .await
            .unwrap();

        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let mut stream =
            SubscriptionStream::new(config, context(store), dt(friday, 0, 0), dt(friday, 23, 59));

        let first = stream.pop_next().await.unwrap().expect("daily bar");
        assert_eq!(first.frontier_time().date_utc(), friday);
        assert!(stream.pop_next().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn daily_fill_forward_skips_closed_equity_dates() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Arc::new(IcebergStore::connect_local(tmp.path()).await.unwrap());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let friday = NaiveDate::from_ymd_opt(2024, 1, 19).unwrap();
        let last = TradeBar::new(
            symbol.clone(),
            dt(friday - chrono::Duration::days(1), 16, 0),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(10), dec!(10), dec!(10), dec!(10), dec!(100)),
        );
        let config = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        let mut stream = SubscriptionProducerState::new(
            config,
            context(store),
            dt(friday, 0, 0),
            dt(friday + chrono::Duration::days(4), 23, 59),
        );
        stream.last_emitted_point = Some(SubscriptionDataPoint::TradeBar(last.clone()));
        stream.last_emitted_time = Some(last.end_time);

        stream.maybe_stage_fill_forward(last.end_time + TimeSpan::from_days(4));

        assert!(stream.prefetched.is_empty());
    }
}
