use crate::normalization::{normalize_quote_bar, normalize_trade_bar, read_factor_rows};
use crate::subscription_data::SubscriptionDataPoint;
use lean_core::{DateTime, Resolution, Result as LeanResult, TickType};
use lean_data::SubscriptionDataConfig;
use lean_storage::{FactorFileEntry, ParquetReader, PathResolver, QueryParams};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

/// LEAN-style subscription reader: advances partition sources by date and emits
/// ordered, normalized data points for one subscription config.
pub struct SubscriptionStream {
    config: SubscriptionDataConfig,
    reader: Arc<ParquetReader>,
    resolver: PathResolver,
    factor_rows: Vec<FactorFileEntry>,
    start: DateTime,
    end: DateTime,
    partition_date: chrono::NaiveDate,
    end_partition_date: chrono::NaiveDate,
    pending: VecDeque<SubscriptionDataPoint>,
    exhausted: bool,
}

impl SubscriptionStream {
    pub fn new(
        config: SubscriptionDataConfig,
        reader: Arc<ParquetReader>,
        resolver: PathResolver,
        start: DateTime,
        end: DateTime,
    ) -> Self {
        let factor_rows = if config.normalization_mode != lean_core::DataNormalizationMode::Raw {
            read_factor_rows(&reader, &resolver, &config.symbol)
        } else {
            Vec::new()
        };
        SubscriptionStream {
            config,
            reader,
            resolver,
            factor_rows,
            start,
            end,
            partition_date: start.date_utc(),
            end_partition_date: end.date_utc(),
            pending: VecDeque::new(),
            exhausted: false,
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
        while self.pending.is_empty() && !self.exhausted {
            if self.partition_date > self.end_partition_date {
                self.exhausted = true;
                break;
            }
            self.load_partition().await?;
            // LEAN advances to the next source date after exhausting the current partition.
            self.partition_date += chrono::Duration::days(1);
        }
        Ok(())
    }

    pub async fn pop_next(&mut self) -> LeanResult<Option<SubscriptionDataPoint>> {
        self.fill_pending().await?;
        Ok(self.pending.pop_front())
    }

    async fn load_partition(&mut self) -> LeanResult<()> {
        let path = self.resolver.market_data_partition(
            &self.config.symbol,
            self.config.resolution,
            self.config.tick_type,
            self.partition_date,
        );
        if !path.exists() {
            return Ok(());
        }

        let day_start = partition_day_start(self.partition_date);
        let day_end = partition_day_end(self.partition_date);
        let is_daily = self.config.resolution == Resolution::Daily;
        let params = if is_daily {
            QueryParams::new().with_symbols(vec![self.config.symbol.id.sid])
        } else {
            QueryParams::new()
                .with_time_range(day_start, day_end)
                .with_symbols(vec![self.config.symbol.id.sid])
        };

        let points = if self.config.resolution.is_tick() {
            self.load_ticks(&path, &params)?
        } else if self.config.tick_type == TickType::Quote {
            self.load_quote_bars(&path, &params).await?
        } else {
            self.load_trade_bars(&path, &params).await?
        };

        for point in points {
            if point.frontier_time() >= self.start && point.frontier_time() <= self.end {
                self.pending.push_back(point);
            }
        }
        Ok(())
    }

    async fn load_trade_bars(
        &self,
        path: &Path,
        params: &QueryParams,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let symbols_by_sid =
            std::collections::HashMap::from([(self.config.symbol.id.sid, self.config.symbol.clone())]);
        let grouped = self
            .reader
            .read_trade_bar_partition_grouped_async(path, &symbols_by_sid, params)
            .await?;
        let mut bars = grouped
            .get(&self.config.symbol.id.sid)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(bars.len());
        for bar in bars.drain(..) {
            let mut bar = bar;
            normalize_trade_bar(
                &mut bar,
                self.config.normalization_mode,
                &self.factor_rows,
            );
            out.push(SubscriptionDataPoint::TradeBar(bar));
        }
        Ok(out)
    }

    async fn load_quote_bars(
        &self,
        path: &Path,
        params: &QueryParams,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let symbols_by_sid =
            std::collections::HashMap::from([(self.config.symbol.id.sid, self.config.symbol.clone())]);
        let grouped = self
            .reader
            .read_quote_bar_partition_grouped_async(path, &symbols_by_sid, params)
            .await?;
        let mut bars = grouped
            .get(&self.config.symbol.id.sid)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::with_capacity(bars.len());
        for bar in bars.drain(..) {
            let mut bar = bar;
            normalize_quote_bar(
                &mut bar,
                self.config.normalization_mode,
                &self.factor_rows,
            );
            out.push(SubscriptionDataPoint::QuoteBar(bar));
        }
        Ok(out)
    }

    fn load_ticks(
        &self,
        path: &Path,
        params: &QueryParams,
    ) -> LeanResult<Vec<SubscriptionDataPoint>> {
        let ticks = self
            .reader
            .read_tick_partition(path, &self.config.symbol, params)?;
        Ok(ticks
            .into_iter()
            .map(SubscriptionDataPoint::Tick)
            .collect())
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
