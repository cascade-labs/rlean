use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Decimal128Array, Int32Array, Int64Array,
    RecordBatch, StringArray, UInt8Array,
};
use arrow_schema::{DataType, Schema};
use async_trait::async_trait;
use chrono::Datelike;
use futures::{stream, StreamExt, TryStreamExt};
use rlean_core::{NanosecondTimestamp, TickType, TimeSpan};
use rlean_data::SubscriptionDataKind;
use rlean_data_tables::{
    Bar, CustomDataPoint, DataMappingMode, FactorFileEntry, MapFileEntry, OptionUniverseRow,
    PartitionTransform, QuoteBar, TableContract, Tick, TradeBar, DECIMAL_PRECISION, DECIMAL_SCALE,
};
use rust_decimal::Decimal;
use tokio::sync::{mpsc, oneshot, Semaphore};
use verglas_sdk::{
    Client, ClientError, ColumnSpec, ConnectOptions, PartitionSpec, QueryStream,
    TableDefinition as VerglasTableDefinition,
};

use crate::{Coverage, HistoricalData, HistoricalDataStore, HistoryRequest, TimeRange};

const TRADE_BARS: &str = "rlean.market_trade_bars";
const QUOTE_BARS: &str = "rlean.market_quote_bars";
const TICKS: &str = "rlean.market_ticks";
const OPTION_UNIVERSE: &str = "rlean.option_universe";
const CUSTOM_POINTS: &str = "rlean.custom_points";
const FACTOR_FILES: &str = "rlean.factor_files";
const MAP_FILES: &str = "rlean.map_files";
const COVERAGE: &str = "rlean.history_coverage";
const BATCH_ROWS: usize = 8_192;
const MAX_CONCURRENT_VERGLAS_IO: usize = 16;
const QUERY_OPEN_ATTEMPTS: usize = 4;
const QUERY_BATCH_DELAY: Duration = Duration::from_millis(2);
const QUERY_BATCH_CAPACITY: usize = 4_096;
const QUERY_BATCH_MAX: usize = 512;

struct CoverageQuery {
    request: HistoryRequest,
    response: oneshot::Sender<Result<Coverage>>,
}

struct MarketDataQuery {
    request: HistoryRequest,
    response: oneshot::Sender<Result<HistoricalData>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MarketDataBatchKey {
    table: &'static str,
    venue: String,
    security_type: String,
    market: String,
    resolution: String,
    start_ns: i64,
    end_ns: i64,
}

/// Canonical historical storage backed by Verglas.
///
/// Queries are executed by the isolated query role and Arrow batches are sent
/// to the isolated write role. The SDK owns transport, pooling, streaming, and
/// idempotency; this type owns only rlean's table contract and predicates.
#[derive(Clone)]
pub struct VerglasHistoricalDataStore {
    client: Client,
    io_permits: Arc<Semaphore>,
    coverage_queries: mpsc::Sender<CoverageQuery>,
    market_data_queries: mpsc::Sender<MarketDataQuery>,
}

impl VerglasHistoricalDataStore {
    pub async fn connect(options: ConnectOptions) -> Result<Self> {
        let client = Client::connect(options)
            .await
            .context("connect to Verglas historical store")?;
        Self::new(client).await
    }

    pub async fn from_env() -> Result<Self> {
        Self::connect(ConnectOptions::from_env()).await
    }

    pub async fn new(client: Client) -> Result<Self> {
        client
            .ensure_table(TRADE_BARS, &contract_definition::<TradeBar>()?)
            .await
            .context("ensure canonical trade-bar table")?;
        client
            .ensure_table(QUOTE_BARS, &contract_definition::<QuoteBar>()?)
            .await
            .context("ensure canonical quote-bar table")?;
        client
            .ensure_table(CUSTOM_POINTS, &contract_definition::<CustomDataPoint>()?)
            .await
            .context("ensure canonical custom-data table")?;
        // `market_ticks` is part of the shared rlean catalog manifest. Its
        // canonical `tick_type` is UInt8, which Verglas's generic create-table
        // surface intentionally does not coerce to a wider integer. Do not let
        // an unrelated bar-only process fail while eagerly ensuring this table;
        // tick queries and appends use the manifest-created table as-is.
        client
            .ensure_table(
                OPTION_UNIVERSE,
                &contract_definition::<OptionUniverseRow>()?,
            )
            .await
            .context("ensure canonical option-universe table")?;
        client
            .ensure_table(FACTOR_FILES, &contract_definition::<FactorFileEntry>()?)
            .await
            .context("ensure canonical factor-file table")?;
        client
            .ensure_table(MAP_FILES, &contract_definition::<MapFileEntry>()?)
            .await
            .context("ensure canonical map-file table")?;
        client
            .ensure_table(COVERAGE, &coverage_definition())
            .await
            .context("ensure historical coverage table")?;
        let io_permits = Arc::new(Semaphore::new(MAX_CONCURRENT_VERGLAS_IO));
        let (coverage_queries, coverage_receiver) = mpsc::channel(QUERY_BATCH_CAPACITY);
        let (market_data_queries, market_data_receiver) = mpsc::channel(QUERY_BATCH_CAPACITY);
        tokio::spawn(run_coverage_query_batcher(
            client.clone(),
            io_permits.clone(),
            coverage_receiver,
        ));
        tokio::spawn(run_market_data_query_batcher(
            client.clone(),
            io_permits.clone(),
            market_data_receiver,
        ));
        Ok(Self {
            client,
            io_permits,
            coverage_queries,
            market_data_queries,
        })
    }

    async fn query_stream(&self, sql: &str) -> Result<QueryStream> {
        query_stream_with_retry(&self.client, sql).await
    }

    fn is_batchable_market_data(request: &HistoryRequest) -> bool {
        request.configuration.data_kind != SubscriptionDataKind::Custom
            && request.configuration.option_chain.is_none()
            && request.configuration.resolution != rlean_core::Resolution::Tick
            && matches!(
                request.configuration.tick_type,
                TickType::Trade | TickType::Quote
            )
    }

    async fn batched_coverage(&self, request: &HistoryRequest) -> Result<Coverage> {
        let (response, receiver) = oneshot::channel();
        self.coverage_queries
            .send(CoverageQuery {
                request: request.clone(),
                response,
            })
            .await
            .context("queue batched historical coverage query")?;
        receiver
            .await
            .context("batched historical coverage worker stopped")?
    }

    async fn batched_market_data(&self, request: &HistoryRequest) -> Result<HistoricalData> {
        let (response, receiver) = oneshot::channel();
        self.market_data_queries
            .send(MarketDataQuery {
                request: request.clone(),
                response,
            })
            .await
            .context("queue batched historical market-data query")?;
        receiver
            .await
            .context("batched historical market-data worker stopped")?
    }

    fn table(request: &HistoryRequest) -> Result<&'static str> {
        if request.configuration.data_kind == SubscriptionDataKind::Custom {
            return Ok(CUSTOM_POINTS);
        }
        if request.configuration.option_chain.is_some() {
            return Ok(OPTION_UNIVERSE);
        }
        if request.configuration.resolution == rlean_core::Resolution::Tick {
            return Ok(TICKS);
        }
        match request.configuration.tick_type {
            TickType::Trade => Ok(TRADE_BARS),
            TickType::Quote => Ok(QUOTE_BARS),
            other => bail!("Verglas historical store does not support {other:?}"),
        }
    }

    fn identity_predicate(request: &HistoryRequest) -> Result<String> {
        if let Some(custom) = request.configuration.custom.as_ref() {
            let query = custom.config.query.merge(&custom.dynamic_query);
            let stored_resolution =
                if request.configuration.resolution == rlean_core::Resolution::Tick {
                    resolution_predicate(rlean_core::Resolution::Tick)
                } else {
                    format!(
                        "({} OR {})",
                        resolution_predicate(request.configuration.resolution),
                        resolution_predicate(rlean_core::Resolution::Tick)
                    )
                };
            let mut predicates = vec![
                format!("provider = '{}'", sql_string(&custom.source_type)),
                format!("feed = '{}'", sql_string(&custom.ticker)),
                stored_resolution,
                format!("venue = '{}'", sql_string(&request.configuration.venue)),
            ];
            if let Some(symbols) = query.symbols.as_ref().filter(|symbols| !symbols.is_empty()) {
                predicates.push(format!(
                    "UPPER(symbol_value) IN ({})",
                    symbols
                        .iter()
                        .map(|symbol| format!("'{}'", sql_string(&symbol.to_ascii_uppercase())))
                        .collect::<Vec<_>>()
                        .join(",")
                ));
            }
            return Ok(predicates.join(" AND "));
        }
        if let Some(metadata) = request.configuration.option_chain.as_ref() {
            return Ok(format!(
                "market = '{}' AND (underlying_value = '{}' OR (underlying_value IS NULL AND symbol_value = '{}'))",
                sql_string(request.configuration.symbol.market().as_str()),
                sql_string(&metadata.underlying_ticker),
                sql_string(&metadata.underlying_ticker),
            ));
        }
        let sid = signed_sid(request.configuration.symbol.sid());
        Ok(format!(
            "symbol_sid = {sid} AND venue = '{}' AND security_type = '{}' AND market = '{}' AND resolution = '{}'",
            sql_string(&request.configuration.venue),
            request.configuration.symbol.security_type(),
            sql_string(request.configuration.symbol.market().as_str()),
            request.configuration.resolution,
        ))
    }

    fn coverage_identity(request: &HistoryRequest) -> Result<String> {
        let sid = signed_sid(request.configuration.symbol.sid());
        Ok(format!(
            "table_name = '{}' AND symbol_sid = {sid} AND venue = '{}' AND resolution = '{}'",
            sql_string(Self::table(request)?),
            sql_string(&request.configuration.venue),
            request.configuration.resolution,
        ))
    }

    async fn fill_option_underlying_prices(
        &self,
        request: &HistoryRequest,
        rows: &mut [OptionUniverseRow],
    ) -> Result<()> {
        let Some(metadata) = request.configuration.option_chain.as_ref() else {
            return Ok(());
        };
        let underlying = request
            .configuration
            .symbol
            .underlying
            .as_deref()
            .cloned()
            .unwrap_or_else(|| {
                rlean_core::Symbol::create_equity(
                    &metadata.underlying_ticker,
                    &rlean_core::Market::usa(),
                )
            });
        let sid = signed_sid(underlying.sid());
        let sql = format!(
            "SELECT * FROM {TRADE_BARS} WHERE symbol_sid = {sid} AND venue = '{}' AND security_type = '{}' AND market = '{}' AND resolution IN ('Minute', 'minute', 'Daily', 'daily') AND end_time_ns > {} AND end_time_ns <= {} ORDER BY end_time_ns",
            sql_string(request.configuration.venue.as_str()),
            underlying.security_type(),
            sql_string(underlying.market().as_str()),
            request.range.start.0,
            request.range.end.0,
        );
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        let mut stream = self
            .query_stream(&sql)
            .await
            .context("query cached option-underlying prices")?;
        let mut closes = HashMap::new();
        loop {
            match stream.try_next().await {
                Ok(Some(batch)) => {
                    for bar in decode_trade_bars(&batch, &underlying)? {
                        closes.insert(bar.end_time.date_utc(), bar.close);
                    }
                }
                Ok(None) => break,
                Err(error) if closes.is_empty() && is_empty_arrow_stream(&error) => break,
                Err(error) => return Err(error).context("stream cached option-underlying prices"),
            }
        }
        for row in rows
            .iter_mut()
            .filter(|row| row.expiration.is_none() && row.close.is_zero())
        {
            if let Some(close) = closes.get(&row.date) {
                row.open = *close;
                row.high = *close;
                row.low = *close;
                row.close = *close;
            }
        }
        Ok(())
    }
}

/// The provider-neutral table contract uses lowercase resolution names, while
/// early native-rlean prototype writes used the Rust enum's title-case Display
/// value. Match both spellings until those already-persisted rows are compacted
/// into the canonical representation. Keeping the column bare on both sides of
/// the `OR` also lets Iceberg/DataFusion prune either identity partition.
fn resolution_predicate(resolution: rlean_core::Resolution) -> String {
    let title_case = resolution.to_string();
    format!(
        "resolution IN ('{}', '{}')",
        title_case,
        title_case.to_ascii_lowercase()
    )
}

/// Preserve all 64 identity bits in Iceberg's signed `long` physical type.
/// Values above `i64::MAX` are represented by their two's-complement signed
/// value and map back losslessly with `value as u64`.
fn signed_sid(sid: u64) -> i64 {
    sid as i64
}

fn retryable_query_error(error: &ClientError) -> bool {
    match error {
        ClientError::RequestTimeout | ClientError::Transport(_) => true,
        ClientError::Http { status, .. } => {
            status.is_server_error() || *status == reqwest::StatusCode::TOO_MANY_REQUESTS
        }
        _ => false,
    }
}

async fn query_stream_with_retry(client: &Client, sql: &str) -> Result<QueryStream> {
    let mut delay = Duration::from_millis(100);
    for attempt in 1..=QUERY_OPEN_ATTEMPTS {
        match client.query_stream(sql).await {
            Ok(stream) => return Ok(stream),
            Err(error) if attempt < QUERY_OPEN_ATTEMPTS && retryable_query_error(&error) => {
                tracing::warn!(
                    attempt,
                    max_attempts = QUERY_OPEN_ATTEMPTS,
                    error = %error,
                    "retrying transient Verglas query-open failure"
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2);
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("Verglas query retry loop always returns")
}

async fn collect_query_batch<T>(receiver: &mut mpsc::Receiver<T>, first: T) -> Vec<T> {
    tokio::time::sleep(QUERY_BATCH_DELAY).await;
    let mut jobs = Vec::with_capacity(QUERY_BATCH_MAX);
    jobs.push(first);
    while jobs.len() < QUERY_BATCH_MAX {
        match receiver.try_recv() {
            Ok(job) => jobs.push(job),
            Err(_) => break,
        }
    }
    jobs
}

async fn run_coverage_query_batcher(
    client: Client,
    io_permits: Arc<Semaphore>,
    mut receiver: mpsc::Receiver<CoverageQuery>,
) {
    while let Some(first) = receiver.recv().await {
        let jobs = collect_query_batch(&mut receiver, first).await;
        let result = execute_coverage_query_batch(&client, &io_permits, &jobs).await;
        match result {
            Ok(mut coverage) => {
                for (job, covered) in jobs.into_iter().zip(coverage.drain(..)) {
                    let _ = job.response.send(Ok(covered));
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                for job in jobs {
                    let _ = job.response.send(Err(anyhow::anyhow!(message.clone())));
                }
            }
        }
    }
}

async fn execute_coverage_query_batch(
    client: &Client,
    io_permits: &Semaphore,
    jobs: &[CoverageQuery],
) -> Result<Vec<Coverage>> {
    tracing::debug!(
        requests = jobs.len(),
        "querying batched historical coverage"
    );
    let predicates = jobs
        .iter()
        .map(|job| {
            Ok(format!(
                "({} AND end_ns > {} AND start_ns < {})",
                VerglasHistoricalDataStore::coverage_identity(&job.request)?,
                job.request.range.start.0,
                job.request.range.end.0,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let sql = format!(
        "SELECT table_name, symbol_sid, venue, resolution, start_ns, end_ns FROM {COVERAGE} WHERE {} ORDER BY table_name, symbol_sid, start_ns",
        predicates.join(" OR ")
    );
    let _permit = io_permits
        .acquire()
        .await
        .context("acquire Verglas I/O permit")?;
    let mut stream = query_stream_with_retry(client, &sql)
        .await
        .context("query batched historical coverage")?;
    type CoverageKey = (String, i64, String, String);
    let mut rows: HashMap<CoverageKey, Vec<TimeRange>> = HashMap::new();
    loop {
        match stream.try_next().await {
            Ok(Some(batch)) => {
                let table_names = utf8(&batch, "table_name")?;
                let symbol_sids = int64(&batch, "symbol_sid")?;
                let venues = utf8(&batch, "venue")?;
                let resolutions = utf8(&batch, "resolution")?;
                let starts = int64(&batch, "start_ns")?;
                let ends = int64(&batch, "end_ns")?;
                for row in 0..batch.num_rows() {
                    let Some(range) = TimeRange::new(
                        NanosecondTimestamp(starts.value(row)),
                        NanosecondTimestamp(ends.value(row)),
                    )
                    .ok() else {
                        continue;
                    };
                    rows.entry((
                        table_names.value(row).to_string(),
                        symbol_sids.value(row),
                        venues.value(row).to_string(),
                        resolutions.value(row).to_string(),
                    ))
                    .or_default()
                    .push(range);
                }
            }
            Ok(None) => break,
            Err(error) if rows.is_empty() && is_empty_arrow_stream(&error) => break,
            Err(error) => return Err(error).context("stream batched historical coverage"),
        }
    }
    jobs.iter()
        .map(|job| {
            let key = (
                VerglasHistoricalDataStore::table(&job.request)?.to_string(),
                signed_sid(job.request.configuration.symbol.sid()),
                job.request.configuration.venue.clone(),
                job.request.configuration.resolution.to_string(),
            );
            let covered = rows
                .get(&key)
                .into_iter()
                .flatten()
                .copied()
                .filter(|range| {
                    range.end > job.request.range.start && range.start < job.request.range.end
                })
                .collect();
            Ok(Coverage { covered })
        })
        .collect()
}

async fn run_market_data_query_batcher(
    client: Client,
    io_permits: Arc<Semaphore>,
    mut receiver: mpsc::Receiver<MarketDataQuery>,
) {
    while let Some(first) = receiver.recv().await {
        let jobs = collect_query_batch(&mut receiver, first).await;
        let mut groups: HashMap<MarketDataBatchKey, Vec<MarketDataQuery>> = HashMap::new();
        for job in jobs {
            match market_data_batch_key(&job.request) {
                Ok(key) => groups.entry(key).or_default().push(job),
                Err(error) => {
                    let _ = job.response.send(Err(error));
                }
            }
        }
        let results = stream::iter(groups)
            .map(|(key, jobs)| {
                let client = client.clone();
                let io_permits = io_permits.clone();
                async move {
                    let result =
                        execute_market_data_query_batch(&client, &io_permits, &key, &jobs).await;
                    (jobs, result)
                }
            })
            .buffer_unordered(MAX_CONCURRENT_VERGLAS_IO)
            .collect::<Vec<_>>()
            .await;
        for (jobs, result) in results {
            match result {
                Ok(mut data) => {
                    for (job, rows) in jobs.into_iter().zip(data.drain(..)) {
                        let _ = job.response.send(Ok(rows));
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    for job in jobs {
                        let _ = job.response.send(Err(anyhow::anyhow!(message.clone())));
                    }
                }
            }
        }
    }
}

fn market_data_batch_key(request: &HistoryRequest) -> Result<MarketDataBatchKey> {
    Ok(MarketDataBatchKey {
        table: VerglasHistoricalDataStore::table(request)?,
        venue: request.configuration.venue.clone(),
        security_type: request.configuration.symbol.security_type().to_string(),
        market: request.configuration.symbol.market().as_str().to_string(),
        resolution: request.configuration.resolution.to_string(),
        start_ns: request.range.start.0,
        end_ns: request.range.end.0,
    })
}

async fn execute_market_data_query_batch(
    client: &Client,
    io_permits: &Semaphore,
    key: &MarketDataBatchKey,
    jobs: &[MarketDataQuery],
) -> Result<Vec<HistoricalData>> {
    let sids = jobs
        .iter()
        .map(|job| signed_sid(job.request.configuration.symbol.sid()))
        .collect::<HashSet<_>>();
    let sid_list = sids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    tracing::debug!(
        table = key.table,
        requests = jobs.len(),
        symbols = sids.len(),
        start_ns = key.start_ns,
        end_ns = key.end_ns,
        "querying batched cached market data"
    );
    let sql = format!(
        "SELECT * FROM {} WHERE symbol_sid IN ({sid_list}) AND venue = '{}' AND security_type = '{}' AND market = '{}' AND {} AND end_time_ns > {} AND end_time_ns <= {} ORDER BY symbol_sid, end_time_ns",
        key.table,
        sql_string(&key.venue),
        sql_string(&key.security_type),
        sql_string(&key.market),
        resolution_predicate_from_str(&key.resolution),
        key.start_ns,
        key.end_ns,
    );
    let _permit = io_permits
        .acquire()
        .await
        .context("acquire Verglas I/O permit")?;
    let mut stream = query_stream_with_retry(client, &sql)
        .await
        .with_context(|| format!("query batched cached history from {}", key.table))?;
    let symbols = jobs
        .iter()
        .map(|job| {
            (
                signed_sid(job.request.configuration.symbol.sid()),
                job.request.configuration.symbol.clone(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut trade_rows: HashMap<i64, Vec<TradeBar>> = HashMap::new();
    let mut quote_rows: HashMap<i64, Vec<QuoteBar>> = HashMap::new();
    loop {
        match stream.try_next().await {
            Ok(Some(batch)) => {
                let batch_sids = int64(&batch, "symbol_sid")?;
                for sid in batch_sids.values().iter().copied().collect::<HashSet<_>>() {
                    let Some(symbol) = symbols.get(&sid) else {
                        continue;
                    };
                    if key.table == TRADE_BARS {
                        trade_rows
                            .entry(sid)
                            .or_default()
                            .extend(decode_trade_bars_for_sid(&batch, symbol, sid)?);
                    } else {
                        quote_rows
                            .entry(sid)
                            .or_default()
                            .extend(decode_quote_bars_for_sid(&batch, symbol, sid)?);
                    }
                }
            }
            Ok(None) => break,
            Err(error)
                if trade_rows.is_empty()
                    && quote_rows.is_empty()
                    && is_empty_arrow_stream(&error) =>
            {
                break;
            }
            Err(error) => return Err(error).context("stream batched cached market data"),
        }
    }
    Ok(jobs
        .iter()
        .map(|job| {
            let sid = signed_sid(job.request.configuration.symbol.sid());
            if key.table == TRADE_BARS {
                HistoricalData::TradeBars(trade_rows.get(&sid).cloned().unwrap_or_default())
            } else {
                HistoricalData::QuoteBars(quote_rows.get(&sid).cloned().unwrap_or_default())
            }
        })
        .collect())
}

fn resolution_predicate_from_str(resolution: &str) -> String {
    format!(
        "resolution IN ('{}', '{}')",
        sql_string(resolution),
        sql_string(&resolution.to_ascii_lowercase())
    )
}

#[async_trait]
impl HistoricalDataStore for VerglasHistoricalDataStore {
    async fn coverage(&self, request: &HistoryRequest) -> Result<Coverage> {
        self.batched_coverage(request).await
    }

    async fn read(&self, request: &HistoryRequest) -> Result<HistoricalData> {
        if Self::is_batchable_market_data(request) {
            return self.batched_market_data(request).await;
        }
        let table = Self::table(request)?;
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        if request.configuration.data_kind == SubscriptionDataKind::Custom {
            let sql = format!(
                "SELECT * FROM {table} WHERE {} AND end_time_ns >= {} AND end_time_ns <= {} ORDER BY end_time_ns, time_ns",
                Self::identity_predicate(request)?,
                request.range.start.0,
                request.range.end.0,
            );
            let mut stream = self
                .query_stream(&sql)
                .await
                .context("query cached custom data")?;
            let custom = request
                .configuration
                .custom
                .as_ref()
                .context("custom subscription has no metadata")?;
            let query = custom.config.query.merge(&custom.dynamic_query);
            let mut rows = Vec::new();
            loop {
                match stream.try_next().await {
                    Ok(Some(batch)) => {
                        rows.extend(decode_custom_points(&batch)?.into_iter().filter(|point| {
                            query.matches_point(point)
                                && custom_point_matches_venue(point, &request.configuration.venue)
                        }))
                    }
                    Ok(None) => break,
                    Err(error) if rows.is_empty() && is_empty_arrow_stream(&error) => break,
                    Err(error) => return Err(error).context("stream cached custom data"),
                }
            }
            tracing::debug!(
                provider = %custom.source_type,
                feed = %custom.ticker,
                resolution = %request.configuration.resolution,
                start_ns = request.range.start.0,
                end_ns = request.range.end.0,
                rows = rows.len(),
                "read cached custom data"
            );
            return Ok(HistoricalData::CustomPoints(rows));
        }
        let (time_column, lower_comparison, upper_comparison) =
            if request.configuration.option_chain.is_some() {
                ("date_ns", ">=", "<")
            } else if request.configuration.resolution == rlean_core::Resolution::Tick {
                ("time_ns", ">=", "<")
            } else {
                ("end_time_ns", ">", "<=")
            };
        let sql = format!(
            "SELECT * FROM {table} WHERE {} AND {time_column} {lower_comparison} {} AND {time_column} {upper_comparison} {} ORDER BY {time_column}",
            Self::identity_predicate(request)?,
            request.range.start.0,
            request.range.end.0,
        );
        let mut stream = self
            .query_stream(&sql)
            .await
            .with_context(|| format!("query cached history from {table}"))?;
        if request.configuration.option_chain.is_some() {
            let mut rows = Vec::new();
            loop {
                match stream.try_next().await {
                    Ok(Some(batch)) => rows.extend(decode_option_universe(&batch)?),
                    Ok(None) => break,
                    Err(error) if rows.is_empty() && is_empty_arrow_stream(&error) => break,
                    Err(error) => return Err(error).context("stream cached option universe"),
                }
            }
            drop(stream);
            // The underlying-price join is a second query. Release this
            // option-universe query slot first so a full permit cohort cannot
            // deadlock while every task waits to acquire a second permit.
            drop(_permit);
            self.fill_option_underlying_prices(request, &mut rows)
                .await?;
            return Ok(HistoricalData::OptionUniverse(rows));
        }
        if request.configuration.resolution == rlean_core::Resolution::Tick {
            let mut rows = Vec::new();
            while let Some(batch) = stream.try_next().await.context("stream cached ticks")? {
                rows.extend(decode_ticks(&batch, &request.configuration.symbol)?);
            }
            return Ok(HistoricalData::Ticks(rows));
        }
        match request.configuration.tick_type {
            TickType::Trade => {
                let mut rows = Vec::new();
                while let Some(batch) = stream
                    .try_next()
                    .await
                    .context("stream cached trade bars")?
                {
                    rows.extend(decode_trade_bars(&batch, &request.configuration.symbol)?);
                }
                Ok(HistoricalData::TradeBars(rows))
            }
            TickType::Quote => {
                let mut rows = Vec::new();
                while let Some(batch) = stream
                    .try_next()
                    .await
                    .context("stream cached quote bars")?
                {
                    rows.extend(decode_quote_bars(&batch, &request.configuration.symbol)?);
                }
                Ok(HistoricalData::QuoteBars(rows))
            }
            other => bail!("Verglas historical store does not support {other:?}"),
        }
    }

    async fn append(
        &self,
        request: &HistoryRequest,
        provider: &str,
        data: &HistoricalData,
    ) -> Result<()> {
        let table = Self::table(request)?;
        let batches = encode_history(request, data)?;
        let key = idempotency_key("data", table, request, provider)?;
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        self.client
            .append_stream(table, stream::iter(batches.into_iter().map(Ok)), &key)
            .await
            .with_context(|| format!("append canonical history to {table}"))?;
        Ok(())
    }

    async fn mark_covered(&self, request: &HistoryRequest, provider: &str) -> Result<()> {
        let sid = signed_sid(request.configuration.symbol.sid());
        let batch = RecordBatch::try_new(
            coverage_schema(),
            vec![
                Arc::new(StringArray::from(vec![Self::table(request)?])) as ArrayRef,
                Arc::new(Int64Array::from(vec![sid])),
                Arc::new(StringArray::from(vec![request
                    .configuration
                    .venue
                    .as_str()])),
                Arc::new(StringArray::from(vec![request
                    .configuration
                    .resolution
                    .to_string()])),
                Arc::new(Int64Array::from(vec![request.range.start.0])),
                Arc::new(Int64Array::from(vec![request.range.end.0])),
                Arc::new(StringArray::from(vec![provider])),
            ],
        )?;
        let key = idempotency_key("coverage", COVERAGE, request, provider)?;
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        self.client
            .append_stream(COVERAGE, stream::iter(vec![Ok(batch)]), &key)
            .await
            .context("persist successful historical coverage")?;
        Ok(())
    }

    async fn read_factor_file(&self, symbol: &rlean_core::Symbol) -> Result<Vec<FactorFileEntry>> {
        let sql = format!(
            "SELECT * FROM {FACTOR_FILES} WHERE market = '{}' AND ticker = '{}' ORDER BY date_ns",
            sql_string(symbol.market().as_str()),
            sql_string(&symbol.permtick)
        );
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        let mut stream = self
            .query_stream(&sql)
            .await
            .context("query cached factor file")?;
        let mut rows = Vec::new();
        loop {
            match stream.try_next().await {
                Ok(Some(batch)) => rows.extend(decode_factor_file(&batch)?),
                Ok(None) => break,
                Err(error) if rows.is_empty() && is_empty_arrow_stream(&error) => break,
                Err(error) => return Err(error).context("stream cached factor file"),
            }
        }
        Ok(rows)
    }

    async fn append_factor_file(
        &self,
        symbol: &rlean_core::Symbol,
        provider: &str,
        rows: &[FactorFileEntry],
    ) -> Result<()> {
        let batch = encode_factor_file(symbol, rows)?;
        let version = rows
            .iter()
            .map(FactorFileEntry::date_ns)
            .max()
            .unwrap_or_default();
        let key = format!(
            "rlean-auxiliary:factor:{provider}:{}:{}:{version}:{}",
            symbol.market(),
            symbol.permtick,
            rows.len()
        );
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        self.client
            .append_stream(FACTOR_FILES, stream::iter(vec![Ok(batch)]), &key)
            .await
            .map_err(|error| anyhow::anyhow!("append factor file: {error}"))?;
        Ok(())
    }

    async fn read_map_file(&self, symbol: &rlean_core::Symbol) -> Result<Vec<MapFileEntry>> {
        let sql = format!(
            "SELECT * FROM {MAP_FILES} WHERE market = '{}' AND permtick = '{}' ORDER BY date_ns",
            sql_string(symbol.market().as_str()),
            sql_string(&symbol.permtick)
        );
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        let mut stream = self
            .query_stream(&sql)
            .await
            .context("query cached map file")?;
        let mut rows = Vec::new();
        loop {
            match stream.try_next().await {
                Ok(Some(batch)) => rows.extend(decode_map_file(&batch)?),
                Ok(None) => break,
                Err(error) if rows.is_empty() && is_empty_arrow_stream(&error) => break,
                Err(error) => return Err(error).context("stream cached map file"),
            }
        }
        Ok(rows)
    }

    async fn append_map_file(
        &self,
        symbol: &rlean_core::Symbol,
        provider: &str,
        rows: &[MapFileEntry],
    ) -> Result<()> {
        let batch = encode_map_file(symbol, rows)?;
        let version = rows
            .iter()
            .map(MapFileEntry::date_ns)
            .max()
            .unwrap_or_default();
        let key = format!(
            "rlean-auxiliary:map:{provider}:{}:{}:{version}:{}",
            symbol.market(),
            symbol.permtick,
            rows.len()
        );
        let _permit = self
            .io_permits
            .acquire()
            .await
            .context("acquire Verglas I/O permit")?;
        self.client
            .append_stream(MAP_FILES, stream::iter(vec![Ok(batch)]), &key)
            .await
            .map_err(|error| anyhow::anyhow!("append map file: {error}"))?;
        Ok(())
    }
}

fn idempotency_key(
    kind: &str,
    table: &str,
    request: &HistoryRequest,
    provider: &str,
) -> Result<String> {
    let sid = signed_sid(request.configuration.symbol.sid());
    Ok(format!(
        "rlean-history:{kind}:{provider}:{table}:{sid}:{}:{}:{}:{}",
        request.configuration.venue,
        request.configuration.resolution,
        request.range.start.0,
        request.range.end.0,
    ))
}

fn encode_history(request: &HistoryRequest, data: &HistoricalData) -> Result<Vec<RecordBatch>> {
    match data {
        HistoricalData::TradeBars(rows) => rows
            .chunks(BATCH_ROWS)
            .map(|chunk| encode_trade_bars(request, chunk))
            .collect(),
        HistoricalData::QuoteBars(rows) => rows
            .chunks(BATCH_ROWS)
            .map(|chunk| encode_quote_bars(request, chunk))
            .collect(),
        HistoricalData::Ticks(rows) => rows
            .chunks(BATCH_ROWS)
            .map(|chunk| encode_ticks(request, chunk))
            .collect(),
        HistoricalData::CustomPoints(_) => {
            bail!("rlean does not persist operator-owned custom data")
        }
        HistoricalData::OptionUniverse(rows) => rows
            .chunks(BATCH_ROWS)
            .map(encode_option_universe)
            .collect(),
        HistoricalData::FutureUniverse(_) | HistoricalData::FundamentalUniverse(_) => {
            bail!("Verglas encoder does not yet support this universe type")
        }
    }
}

fn encode_trade_bars(request: &HistoryRequest, rows: &[TradeBar]) -> Result<RecordBatch> {
    let sid = signed_sid(request.configuration.symbol.sid());
    let decimals = |values: Vec<Decimal>| -> Result<ArrayRef> {
        Ok(Arc::new(
            Decimal128Array::from(values.into_iter().map(scale_decimal).collect::<Vec<_>>())
                .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    RecordBatch::try_new(
        TradeBar::schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.time.0),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.end_time.0),
            )),
            Arc::new(Int64Array::from(vec![sid; rows.len()])),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.symbol.value()),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.venue.as_deref())
                    .collect::<Vec<_>>(),
            )),
            decimals(rows.iter().map(|row| row.open).collect())?,
            decimals(rows.iter().map(|row| row.high).collect())?,
            decimals(rows.iter().map(|row| row.low).collect())?,
            decimals(rows.iter().map(|row| row.close).collect())?,
            decimals(rows.iter().map(|row| row.volume).collect())?,
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.period.nanos),
            )),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .symbol
                    .security_type()
                    .to_string();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .symbol
                    .market()
                    .as_str();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .resolution
                    .to_string();
                rows.len()
            ])),
            Arc::new(Date32Array::from_iter_values(
                rows.iter().map(|row| date32(row.time)),
            )),
        ],
    )
    .context("encode canonical trade bars")
}

fn encode_quote_bars(request: &HistoryRequest, rows: &[QuoteBar]) -> Result<RecordBatch> {
    let sid = signed_sid(request.configuration.symbol.sid());
    let side = |select: fn(&Bar) -> Decimal, bid: bool| -> Result<ArrayRef> {
        let values = rows
            .iter()
            .map(|row| {
                let bar = if bid {
                    row.bid.as_ref()
                } else {
                    row.ask.as_ref()
                };
                bar.map(|value| scale_decimal(select(value)))
            })
            .collect::<Vec<_>>();
        Ok(Arc::new(
            Decimal128Array::from(values)
                .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    let decimals = |values: Vec<Decimal>| -> Result<ArrayRef> {
        Ok(Arc::new(
            Decimal128Array::from(values.into_iter().map(scale_decimal).collect::<Vec<_>>())
                .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    RecordBatch::try_new(
        QuoteBar::schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.time.0),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.end_time.0),
            )),
            Arc::new(Int64Array::from(vec![sid; rows.len()])),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.symbol.value()),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.venue.as_deref())
                    .collect::<Vec<_>>(),
            )),
            side(|bar| bar.open, true)?,
            side(|bar| bar.high, true)?,
            side(|bar| bar.low, true)?,
            side(|bar| bar.close, true)?,
            side(|bar| bar.open, false)?,
            side(|bar| bar.high, false)?,
            side(|bar| bar.low, false)?,
            side(|bar| bar.close, false)?,
            decimals(rows.iter().map(|row| row.last_bid_size).collect())?,
            decimals(rows.iter().map(|row| row.last_ask_size).collect())?,
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.period.nanos),
            )),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .symbol
                    .security_type()
                    .to_string();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .symbol
                    .market()
                    .as_str();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .resolution
                    .to_string();
                rows.len()
            ])),
            Arc::new(Date32Array::from_iter_values(
                rows.iter().map(|row| date32(row.time)),
            )),
        ],
    )
    .context("encode canonical quote bars")
}

fn decode_trade_bars(batch: &RecordBatch, symbol: &rlean_core::Symbol) -> Result<Vec<TradeBar>> {
    decode_trade_bars_rows(batch, symbol, |_| true)
}

fn decode_trade_bars_for_sid(
    batch: &RecordBatch,
    symbol: &rlean_core::Symbol,
    requested_sid: i64,
) -> Result<Vec<TradeBar>> {
    let sids = int64(batch, "symbol_sid")?;
    decode_trade_bars_rows(batch, symbol, |row| sids.value(row) == requested_sid)
}

fn decode_trade_bars_rows(
    batch: &RecordBatch,
    symbol: &rlean_core::Symbol,
    include: impl Fn(usize) -> bool,
) -> Result<Vec<TradeBar>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let open = decimal(batch, "open")?;
    let high = decimal(batch, "high")?;
    let low = decimal(batch, "low")?;
    let close = decimal(batch, "close")?;
    let volume = decimal(batch, "volume")?;
    let period = int64(batch, "period_ns")?;
    Ok((0..batch.num_rows())
        .filter(|row| include(*row))
        .map(|row| TradeBar {
            symbol: symbol.clone(),
            venue: string_at(batch, "venue", row),
            time: NanosecondTimestamp(time.value(row)),
            end_time: NanosecondTimestamp(end_time.value(row)),
            open: decimal_value(open.value(row)),
            high: decimal_value(high.value(row)),
            low: decimal_value(low.value(row)),
            close: decimal_value(close.value(row)),
            volume: decimal_value(volume.value(row)),
            period: TimeSpan::from_nanos(period.value(row)),
        })
        .collect())
}

fn decode_custom_points(batch: &RecordBatch) -> Result<Vec<CustomDataPoint>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let value = decimal(batch, "value")?;
    let fields = batch
        .column_by_name("fields_json")
        .context("missing fields_json column")?
        .as_any()
        .downcast_ref::<StringArray>()
        .context("fields_json is not Utf8")?;
    (0..batch.num_rows())
        .map(|row| {
            let fields =
                serde_json::from_str::<std::collections::HashMap<String, serde_json::Value>>(
                    fields.value(row),
                )
                .with_context(|| format!("invalid custom fields_json at row {row}"))?;
            Ok(CustomDataPoint {
                time: NanosecondTimestamp(time.value(row)),
                end_time: NanosecondTimestamp(end_time.value(row)),
                value: decimal_value(value.value(row)),
                venue: string_at(batch, "venue", row),
                symbol: string_at(batch, "symbol_value", row),
                fields: Arc::new(fields),
            })
        })
        .collect()
}

fn custom_point_matches_venue(point: &CustomDataPoint, expected: &str) -> bool {
    let expected = expected.trim();
    expected.is_empty()
        || point
            .venue
            .as_deref()
            .is_some_and(|venue| venue.eq_ignore_ascii_case(expected))
        || [
            "venue",
            "data_venue",
            "exchange",
            "exchange_code",
            "market_center",
        ]
        .iter()
        .any(|field| {
            point
                .fields
                .get(*field)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|value| value.eq_ignore_ascii_case(expected))
        })
}

fn encode_ticks(request: &HistoryRequest, rows: &[Tick]) -> Result<RecordBatch> {
    let sid = signed_sid(request.configuration.symbol.sid());
    let decimals = |values: Vec<Decimal>| -> Result<ArrayRef> {
        Ok(Arc::new(
            Decimal128Array::from(values.into_iter().map(scale_decimal).collect::<Vec<_>>())
                .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    RecordBatch::try_new(
        Tick::schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| row.time.0),
            )),
            Arc::new(Int64Array::from(vec![sid; rows.len()])),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.symbol.value()),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.venue.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(UInt8Array::from_iter_values(rows.iter().map(
                |row| match row.tick_type {
                    TickType::Trade => 0,
                    TickType::Quote => 1,
                    TickType::OpenInterest => 2,
                },
            ))),
            decimals(rows.iter().map(|row| row.value).collect())?,
            decimals(rows.iter().map(|row| row.quantity).collect())?,
            decimals(rows.iter().map(|row| row.bid_price).collect())?,
            decimals(rows.iter().map(|row| row.ask_price).collect())?,
            decimals(rows.iter().map(|row| row.bid_size).collect())?,
            decimals(rows.iter().map(|row| row.ask_size).collect())?,
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.exchange.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.sale_condition.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(BooleanArray::from(
                rows.iter().map(|row| row.suspicious).collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .symbol
                    .security_type()
                    .to_string();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .symbol
                    .market()
                    .as_str();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                request
                    .configuration
                    .resolution
                    .to_string();
                rows.len()
            ])),
            Arc::new(Date32Array::from_iter_values(
                rows.iter().map(|row| date32(row.time)),
            )),
        ],
    )
    .context("encode canonical ticks")
}

fn decode_ticks(batch: &RecordBatch, symbol: &rlean_core::Symbol) -> Result<Vec<Tick>> {
    let time = int64(batch, "time_ns")?;
    let tick_type = batch
        .column_by_name("tick_type")
        .context("missing tick_type column")?
        .as_any()
        .downcast_ref::<UInt8Array>()
        .context("tick_type is not UInt8")?;
    let value = decimal(batch, "value")?;
    let quantity = decimal(batch, "quantity")?;
    let bid_price = decimal(batch, "bid_price")?;
    let ask_price = decimal(batch, "ask_price")?;
    let bid_size = decimal(batch, "bid_size")?;
    let ask_size = decimal(batch, "ask_size")?;
    let suspicious = batch
        .column_by_name("suspicious")
        .context("missing suspicious column")?
        .as_any()
        .downcast_ref::<BooleanArray>()
        .context("suspicious is not Boolean")?;
    Ok((0..batch.num_rows())
        .map(|row| Tick {
            symbol: symbol.clone(),
            venue: string_at(batch, "venue", row),
            time: NanosecondTimestamp(time.value(row)),
            tick_type: match tick_type.value(row) {
                0 => TickType::Trade,
                1 => TickType::Quote,
                _ => TickType::OpenInterest,
            },
            value: decimal_value(value.value(row)),
            quantity: decimal_value(quantity.value(row)),
            bid_price: decimal_value(bid_price.value(row)),
            ask_price: decimal_value(ask_price.value(row)),
            bid_size: decimal_value(bid_size.value(row)),
            ask_size: decimal_value(ask_size.value(row)),
            exchange: string_at(batch, "exchange", row),
            sale_condition: string_at(batch, "sale_condition", row),
            suspicious: suspicious.value(row),
        })
        .collect())
}

fn encode_option_universe(rows: &[OptionUniverseRow]) -> Result<RecordBatch> {
    let required_decimals = |values: Vec<Decimal>| -> Result<ArrayRef> {
        Ok(Arc::new(
            Decimal128Array::from(values.into_iter().map(scale_decimal).collect::<Vec<_>>())
                .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    let nullable_decimals = |values: Vec<Option<Decimal>>| -> Result<ArrayRef> {
        Ok(Arc::new(
            Decimal128Array::from(
                values
                    .into_iter()
                    .map(|value| value.map(scale_decimal))
                    .collect::<Vec<_>>(),
            )
            .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    let date_ns = |date: chrono::NaiveDate| {
        date.and_hms_opt(0, 0, 0)
            .and_then(|v| v.and_utc().timestamp_nanos_opt())
            .unwrap_or_default()
    };
    RecordBatch::try_new(
        OptionUniverseRow::schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|row| date_ns(row.date)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.market.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.security_type.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.symbol_sid.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.symbol_value.as_str()),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.underlying_sid.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.underlying_value.as_deref())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Int64Array::from(
                rows.iter()
                    .map(|row| row.expiration.map(date_ns))
                    .collect::<Vec<_>>(),
            )),
            nullable_decimals(rows.iter().map(|row| row.strike).collect())?,
            Arc::new(StringArray::from(
                rows.iter()
                    .map(|row| row.right.as_deref())
                    .collect::<Vec<_>>(),
            )),
            required_decimals(rows.iter().map(|row| row.open).collect())?,
            required_decimals(rows.iter().map(|row| row.high).collect())?,
            required_decimals(rows.iter().map(|row| row.low).collect())?,
            required_decimals(rows.iter().map(|row| row.close).collect())?,
            required_decimals(rows.iter().map(|row| row.volume).collect())?,
            nullable_decimals(rows.iter().map(|row| row.open_interest).collect())?,
            nullable_decimals(rows.iter().map(|row| row.implied_volatility).collect())?,
            nullable_decimals(rows.iter().map(|row| row.delta).collect())?,
            nullable_decimals(rows.iter().map(|row| row.gamma).collect())?,
            nullable_decimals(rows.iter().map(|row| row.vega).collect())?,
            nullable_decimals(rows.iter().map(|row| row.theta).collect())?,
            nullable_decimals(rows.iter().map(|row| row.rho).collect())?,
            Arc::new(Date32Array::from_iter_values(
                rows.iter().map(|row| row.date.num_days_from_ce() - 719_163),
            )),
        ],
    )
    .context("encode canonical option universe")
}

fn decode_option_universe(batch: &RecordBatch) -> Result<Vec<OptionUniverseRow>> {
    let date_ns = int64(batch, "date_ns")?;
    let expiration_ns = int64(batch, "expiration_ns")?;
    let required = |name| decimal(batch, name);
    let strike = required("strike")?;
    let open = required("open")?;
    let high = required("high")?;
    let low = required("low")?;
    let close = required("close")?;
    let volume = required("volume")?;
    let oi = required("open_interest")?;
    let iv = required("implied_volatility")?;
    let delta = required("delta")?;
    let gamma = required("gamma")?;
    let vega = required("vega")?;
    let theta = required("theta")?;
    let rho = required("rho")?;
    let date = |ns: i64| chrono::DateTime::from_timestamp_nanos(ns).date_naive();
    let optional_decimal = |array: &Decimal128Array, row| {
        (!array.is_null(row)).then(|| decimal_value(array.value(row)))
    };
    Ok((0..batch.num_rows())
        .map(|row| OptionUniverseRow {
            date: date(date_ns.value(row)),
            market: string_at(batch, "market", row).unwrap_or_default(),
            security_type: string_at(batch, "security_type", row).unwrap_or_default(),
            symbol_sid: string_at(batch, "symbol_sid", row).unwrap_or_default(),
            symbol_value: string_at(batch, "symbol_value", row).unwrap_or_default(),
            underlying_sid: string_at(batch, "underlying_sid", row),
            underlying_value: string_at(batch, "underlying_value", row),
            expiration: (!expiration_ns.is_null(row)).then(|| date(expiration_ns.value(row))),
            strike: optional_decimal(strike, row),
            right: string_at(batch, "right", row),
            open: decimal_value(open.value(row)),
            high: decimal_value(high.value(row)),
            low: decimal_value(low.value(row)),
            close: decimal_value(close.value(row)),
            volume: decimal_value(volume.value(row)),
            open_interest: optional_decimal(oi, row),
            implied_volatility: optional_decimal(iv, row),
            delta: optional_decimal(delta, row),
            gamma: optional_decimal(gamma, row),
            vega: optional_decimal(vega, row),
            theta: optional_decimal(theta, row),
            rho: optional_decimal(rho, row),
        })
        .collect())
}

fn encode_factor_file(
    symbol: &rlean_core::Symbol,
    rows: &[FactorFileEntry],
) -> Result<RecordBatch> {
    let decimal_array = |values: Vec<Decimal>| -> Result<ArrayRef> {
        Ok(Arc::new(
            Decimal128Array::from(values.into_iter().map(scale_decimal).collect::<Vec<_>>())
                .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)?,
        ))
    };
    RecordBatch::try_new(
        FactorFileEntry::schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(FactorFileEntry::date_ns),
            )),
            decimal_array(rows.iter().map(|row| row.price_factor).collect())?,
            decimal_array(rows.iter().map(|row| row.split_factor).collect())?,
            decimal_array(rows.iter().map(|row| row.reference_price).collect())?,
            Arc::new(StringArray::from(vec![
                symbol.market().as_str();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                symbol.permtick.as_ref();
                rows.len()
            ])),
        ],
    )
    .context("encode factor file")
}

fn decode_factor_file(batch: &RecordBatch) -> Result<Vec<FactorFileEntry>> {
    let date_ns = int64(batch, "date_ns")?;
    let price = decimal(batch, "price_factor")?;
    let split = decimal(batch, "split_factor")?;
    let reference = decimal(batch, "reference_price")?;
    Ok((0..batch.num_rows())
        .map(|row| FactorFileEntry {
            date: chrono::DateTime::from_timestamp_nanos(date_ns.value(row)).date_naive(),
            price_factor: decimal_value(price.value(row)),
            split_factor: decimal_value(split.value(row)),
            reference_price: decimal_value(reference.value(row)),
        })
        .collect())
}

fn encode_map_file(symbol: &rlean_core::Symbol, rows: &[MapFileEntry]) -> Result<RecordBatch> {
    RecordBatch::try_new(
        MapFileEntry::schema(),
        vec![
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(MapFileEntry::date_ns),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.mapped_symbol.as_str()),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| row.primary_exchange_code.as_str()),
            )),
            Arc::new(Int32Array::from(
                rows.iter()
                    .map(|row| row.data_mapping_mode.map(|mode| mode as i32))
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec![
                symbol.market().as_str();
                rows.len()
            ])),
            Arc::new(StringArray::from(vec![
                symbol.permtick.as_ref();
                rows.len()
            ])),
        ],
    )
    .context("encode map file")
}

fn decode_map_file(batch: &RecordBatch) -> Result<Vec<MapFileEntry>> {
    let date_ns = int64(batch, "date_ns")?;
    let mode = batch
        .column_by_name("data_mapping_mode")
        .context("missing data_mapping_mode")?
        .as_any()
        .downcast_ref::<Int32Array>()
        .context("data_mapping_mode is not Int32")?;
    Ok((0..batch.num_rows())
        .map(|row| MapFileEntry {
            date: chrono::DateTime::from_timestamp_nanos(date_ns.value(row)).date_naive(),
            mapped_symbol: string_at(batch, "mapped_symbol", row).unwrap_or_default(),
            primary_exchange_code: string_at(batch, "primary_exchange_code", row)
                .unwrap_or_default(),
            data_mapping_mode: (!mode.is_null(row))
                .then(|| DataMappingMode::try_from(mode.value(row)).ok())
                .flatten(),
        })
        .collect())
}

fn decode_quote_bars(batch: &RecordBatch, symbol: &rlean_core::Symbol) -> Result<Vec<QuoteBar>> {
    decode_quote_bars_rows(batch, symbol, |_| true)
}

fn decode_quote_bars_for_sid(
    batch: &RecordBatch,
    symbol: &rlean_core::Symbol,
    requested_sid: i64,
) -> Result<Vec<QuoteBar>> {
    let sids = int64(batch, "symbol_sid")?;
    decode_quote_bars_rows(batch, symbol, |row| sids.value(row) == requested_sid)
}

fn decode_quote_bars_rows(
    batch: &RecordBatch,
    symbol: &rlean_core::Symbol,
    include: impl Fn(usize) -> bool,
) -> Result<Vec<QuoteBar>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let bid_open = decimal(batch, "bid_open")?;
    let bid_high = decimal(batch, "bid_high")?;
    let bid_low = decimal(batch, "bid_low")?;
    let bid_close = decimal(batch, "bid_close")?;
    let ask_open = decimal(batch, "ask_open")?;
    let ask_high = decimal(batch, "ask_high")?;
    let ask_low = decimal(batch, "ask_low")?;
    let ask_close = decimal(batch, "ask_close")?;
    let bid_size = decimal(batch, "last_bid_size")?;
    let ask_size = decimal(batch, "last_ask_size")?;
    let period = int64(batch, "period_ns")?;
    Ok((0..batch.num_rows())
        .filter(|row| include(*row))
        .map(|row| QuoteBar {
            symbol: symbol.clone(),
            venue: string_at(batch, "venue", row),
            time: NanosecondTimestamp(time.value(row)),
            end_time: NanosecondTimestamp(end_time.value(row)),
            bid: bar_at(bid_open, bid_high, bid_low, bid_close, row),
            ask: bar_at(ask_open, ask_high, ask_low, ask_close, row),
            last_bid_size: decimal_value(bid_size.value(row)),
            last_ask_size: decimal_value(ask_size.value(row)),
            period: TimeSpan::from_nanos(period.value(row)),
        })
        .collect())
}

fn bar_at(
    open: &Decimal128Array,
    high: &Decimal128Array,
    low: &Decimal128Array,
    close: &Decimal128Array,
    row: usize,
) -> Option<Bar> {
    (!open.is_null(row) && !high.is_null(row) && !low.is_null(row) && !close.is_null(row)).then(
        || {
            Bar::new(
                decimal_value(open.value(row)),
                decimal_value(high.value(row)),
                decimal_value(low.value(row)),
                decimal_value(close.value(row)),
            )
        },
    )
}

fn int64<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing {name} column"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .with_context(|| format!("{name} is not Int64"))
}

fn utf8<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing {name} column"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .with_context(|| format!("{name} is not Utf8"))
}

fn decimal<'a>(batch: &'a RecordBatch, name: &str) -> Result<&'a Decimal128Array> {
    batch
        .column_by_name(name)
        .with_context(|| format!("missing {name} column"))?
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .with_context(|| format!("{name} is not Decimal128"))
}

fn string_at(batch: &RecordBatch, name: &str, row: usize) -> Option<String> {
    batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref::<StringArray>())
        .filter(|values| !values.is_null(row))
        .map(|values| values.value(row).to_owned())
}

fn scale_decimal(value: Decimal) -> i128 {
    let scale = DECIMAL_SCALE as u32;
    let value = value.round_dp(scale);
    value.mantissa() * 10_i128.pow(scale - value.scale())
}

fn decimal_value(value: i128) -> Decimal {
    Decimal::from_i128_with_scale(value, DECIMAL_SCALE as u32)
}

fn date32(value: NanosecondTimestamp) -> i32 {
    value.date_utc().num_days_from_ce() - 719_163
}

fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn is_empty_arrow_stream(error: &impl std::fmt::Display) -> bool {
    let message = error.to_string();
    message.contains("Unexpected End of Stream") || message.contains("Unexpected end of stream")
}

fn coverage_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        arrow_schema::Field::new("table_name", DataType::Utf8, false),
        arrow_schema::Field::new("symbol_sid", DataType::Int64, false),
        arrow_schema::Field::new("venue", DataType::Utf8, false),
        arrow_schema::Field::new("resolution", DataType::Utf8, false),
        arrow_schema::Field::new("start_ns", DataType::Int64, false),
        arrow_schema::Field::new("end_ns", DataType::Int64, false),
        arrow_schema::Field::new("provider", DataType::Utf8, false),
    ]))
}

fn coverage_definition() -> VerglasTableDefinition {
    VerglasTableDefinition {
        schema: vec![
            ColumnSpec::required("table_name", "utf8"),
            ColumnSpec::required("symbol_sid", "int64"),
            ColumnSpec::required("venue", "utf8"),
            ColumnSpec::required("resolution", "utf8"),
            ColumnSpec::required("start_ns", "int64"),
            ColumnSpec::required("end_ns", "int64"),
            ColumnSpec::required("provider", "utf8"),
        ],
        partitions: vec![
            PartitionSpec::identity("table_name"),
            PartitionSpec::identity("resolution"),
        ],
    }
}

fn contract_definition<T: TableContract>() -> Result<VerglasTableDefinition> {
    let schema = T::schema();
    let columns = schema
        .fields()
        .iter()
        .map(|field| {
            let type_name = match field.data_type() {
                DataType::Int64 => "int64".to_owned(),
                DataType::Int32 => "int32".to_owned(),
                DataType::Utf8 => "utf8".to_owned(),
                DataType::Boolean => "boolean".to_owned(),
                DataType::Date32 => "date32".to_owned(),
                DataType::Decimal128(precision, scale) => {
                    format!("decimal128({precision},{scale})")
                }
                other => bail!("unsupported canonical Arrow type {other}"),
            };
            Ok(if field.is_nullable() {
                ColumnSpec::nullable(field.name(), type_name)
            } else {
                ColumnSpec::required(field.name(), type_name)
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let partitions = T::PARTITION_FIELDS
        .iter()
        .map(|field| match field.transform {
            PartitionTransform::Identity => PartitionSpec::identity(field.source),
            PartitionTransform::Month => PartitionSpec::month(field.source),
        })
        .collect();
    Ok(VerglasTableDefinition {
        schema: columns,
        partitions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{DataNormalizationMode, Market, Resolution, Symbol};
    use rlean_data::{
        CustomDataConfig, CustomDataQuery, CustomSubscriptionMetadata, SubscriptionDataConfig,
    };
    use rust_decimal_macros::dec;
    use std::collections::HashMap;

    fn request(tick_type: TickType) -> HistoryRequest {
        let mut configuration = SubscriptionDataConfig::new_equity(
            Symbol::create_equity("SPY", &Market::usa()),
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        configuration.set_tick_type(tick_type);
        HistoryRequest::new(
            configuration,
            NanosecondTimestamp::from_secs(1),
            NanosecondTimestamp::from_secs(2),
        )
        .unwrap()
    }

    fn custom_request() -> HistoryRequest {
        let source_type = "unusual_whales".to_string();
        let ticker = "market_tide".to_string();
        let mut properties = HashMap::new();
        properties.insert("symbols".to_string(), "SPY,QQQ".to_string());
        let query = CustomDataQuery::from_properties(&properties);
        let metadata = CustomSubscriptionMetadata {
            source_type: source_type.clone(),
            ticker: ticker.clone(),
            config: CustomDataConfig {
                ticker: ticker.clone(),
                source_type: source_type.clone(),
                resolution: Resolution::Minute,
                properties,
                query: query.clone(),
            },
            dynamic_query: query,
        };
        HistoryRequest::new(
            SubscriptionDataConfig::new_custom(
                Symbol::create_base(&source_type, &ticker, &Market::usa()),
                Resolution::Minute,
                metadata,
            ),
            NanosecondTimestamp::from_secs(1),
            NanosecondTimestamp::from_secs(2),
        )
        .unwrap()
    }

    #[test]
    fn canonical_trade_bar_round_trip() {
        let request = request(TickType::Trade);
        let bar = TradeBar {
            symbol: request.configuration.symbol.clone(),
            venue: Some("usa".to_owned()),
            time: NanosecondTimestamp::from_secs(1),
            end_time: NanosecondTimestamp::from_secs(2),
            open: dec!(100.01),
            high: dec!(101.02),
            low: dec!(99.03),
            close: dec!(100.04),
            volume: dec!(1234),
            period: TimeSpan::ONE_SECOND,
        };
        let batch = encode_trade_bars(&request, std::slice::from_ref(&bar)).unwrap();
        assert_eq!(decode_trade_bars(&batch, &bar.symbol).unwrap(), vec![bar]);
    }

    #[test]
    fn canonical_quote_bar_round_trip_preserves_nullable_sides() {
        let request = request(TickType::Quote);
        let bar = QuoteBar {
            symbol: request.configuration.symbol.clone(),
            venue: Some("usa".to_owned()),
            time: NanosecondTimestamp::from_secs(1),
            end_time: NanosecondTimestamp::from_secs(2),
            bid: Some(Bar::new(dec!(99.01), dec!(99.05), dec!(99.00), dec!(99.04))),
            ask: None,
            last_bid_size: dec!(12),
            last_ask_size: dec!(0),
            period: TimeSpan::ONE_SECOND,
        };
        let batch = encode_quote_bars(&request, std::slice::from_ref(&bar)).unwrap();
        assert_eq!(decode_quote_bars(&batch, &bar.symbol).unwrap(), vec![bar]);
    }

    #[test]
    fn query_predicates_push_down_full_subscription_identity() {
        let request = request(TickType::Trade);
        let predicate = VerglasHistoricalDataStore::identity_predicate(&request).unwrap();
        assert!(predicate.contains("symbol_sid = "));
        assert!(predicate.contains("venue = 'usa'"));
        assert!(predicate.contains("security_type = 'Equity'"));
        assert!(predicate.contains("market = 'usa'"));
        assert!(predicate.contains("resolution = 'Daily'"));
    }

    #[test]
    fn custom_query_predicate_uses_provider_feed_venue_resolution_and_symbols() {
        let predicate = VerglasHistoricalDataStore::identity_predicate(&custom_request()).unwrap();
        assert!(predicate.contains("provider = 'unusual_whales'"));
        assert!(predicate.contains("feed = 'market_tide'"));
        assert!(predicate.contains("venue = 'unusual_whales'"));
        assert!(predicate.contains("resolution IN ('Minute', 'minute')"));
        assert!(predicate.contains("resolution IN ('Tick', 'tick')"));
        assert!(predicate.contains("UPPER(symbol_value) IN ('SPY','QQQ')"));
    }

    #[test]
    fn canonical_custom_point_decodes_json_symbol_and_venue() {
        let schema = CustomDataPoint::schema();
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![1])),
                Arc::new(Int64Array::from(vec![2])),
                Arc::new(
                    Decimal128Array::from(vec![scale_decimal(dec!(42.5))])
                        .with_precision_and_scale(DECIMAL_PRECISION, DECIMAL_SCALE)
                        .unwrap(),
                ),
                Arc::new(StringArray::from(vec![r#"{"ticker":"SPY"}"#])),
                Arc::new(StringArray::from(vec![Some("unusual_whales")])),
                Arc::new(Int64Array::from(vec![None])),
                Arc::new(StringArray::from(vec![Some("SPY")])),
                Arc::new(StringArray::from(vec!["unusual_whales"])),
                Arc::new(StringArray::from(vec!["market_tide"])),
                Arc::new(StringArray::from(vec!["minute"])),
                Arc::new(Date32Array::from(vec![0])),
            ],
        )
        .unwrap();

        let points = decode_custom_points(&batch).unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].time, NanosecondTimestamp(1));
        assert_eq!(points[0].end_time, NanosecondTimestamp(2));
        assert_eq!(points[0].value, dec!(42.5));
        assert_eq!(points[0].symbol.as_deref(), Some("SPY"));
        assert_eq!(points[0].venue.as_deref(), Some("unusual_whales"));
        assert_eq!(points[0].fields["ticker"], "SPY");
    }
}
