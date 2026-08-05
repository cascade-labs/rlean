use std::sync::Arc;

use anyhow::{bail, Context, Result};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Decimal128Array, Int32Array, Int64Array,
    RecordBatch, StringArray, UInt8Array,
};
use arrow_schema::{DataType, Schema};
use async_trait::async_trait;
use chrono::Datelike;
use futures::{stream, TryStreamExt};
use rlean_core::{NanosecondTimestamp, TickType, TimeSpan};
use rlean_data_tables::{
    Bar, DataMappingMode, FactorFileEntry, MapFileEntry, OptionUniverseRow, PartitionTransform,
    QuoteBar, TableContract, Tick, TradeBar, DECIMAL_PRECISION, DECIMAL_SCALE,
};
use rust_decimal::Decimal;
use verglas_sdk::{
    Client, ColumnSpec, ConnectOptions, PartitionSpec, TableDefinition as VerglasTableDefinition,
};

use crate::{Coverage, HistoricalData, HistoricalDataStore, HistoryRequest, TimeRange};

const TRADE_BARS: &str = "rlean.market_trade_bars";
const QUOTE_BARS: &str = "rlean.market_quote_bars";
const TICKS: &str = "rlean.market_ticks";
const OPTION_UNIVERSE: &str = "rlean.option_universe";
const FACTOR_FILES: &str = "rlean.factor_files";
const MAP_FILES: &str = "rlean.map_files";
const COVERAGE: &str = "rlean.history_coverage";
const BATCH_ROWS: usize = 8_192;

/// Canonical historical storage backed by Verglas.
///
/// Queries are executed by the isolated query role and Arrow batches are sent
/// to the isolated write role. The SDK owns transport, pooling, streaming, and
/// idempotency; this type owns only rlean's table contract and predicates.
#[derive(Clone)]
pub struct VerglasHistoricalDataStore {
    client: Client,
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
        Ok(Self { client })
    }

    fn table(request: &HistoryRequest) -> Result<&'static str> {
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
        if let Some(metadata) = request.configuration.option_chain.as_ref() {
            return Ok(format!(
                "market = '{}' AND (underlying_value = '{}' OR (underlying_value IS NULL AND symbol_value = '{}'))",
                sql_string(request.configuration.symbol.market().as_str()),
                sql_string(&metadata.underlying_ticker),
                sql_string(&metadata.underlying_ticker),
            ));
        }
        let sid = i64::try_from(request.configuration.symbol.sid())
            .context("symbol SID exceeds the canonical signed 64-bit contract")?;
        Ok(format!(
            "symbol_sid = {sid} AND venue = '{}' AND security_type = '{}' AND market = '{}' AND resolution = '{}'",
            sql_string(&request.configuration.venue),
            request.configuration.symbol.security_type(),
            sql_string(request.configuration.symbol.market().as_str()),
            request.configuration.resolution,
        ))
    }

    fn coverage_identity(request: &HistoryRequest) -> Result<String> {
        let sid = i64::try_from(request.configuration.symbol.sid())
            .context("symbol SID exceeds the canonical signed 64-bit contract")?;
        Ok(format!(
            "table_name = '{}' AND symbol_sid = {sid} AND venue = '{}' AND resolution = '{}'",
            sql_string(Self::table(request)?),
            sql_string(&request.configuration.venue),
            request.configuration.resolution,
        ))
    }
}

#[async_trait]
impl HistoricalDataStore for VerglasHistoricalDataStore {
    async fn coverage(&self, request: &HistoryRequest) -> Result<Coverage> {
        let sql = format!(
            "SELECT start_ns, end_ns FROM {COVERAGE} WHERE {} AND end_ns > {} AND start_ns < {} ORDER BY start_ns",
            Self::coverage_identity(request)?,
            request.range.start.0,
            request.range.end.0,
        );
        let mut stream = self
            .client
            .query_stream(&sql)
            .await
            .context("query historical coverage")?;
        let mut covered = Vec::new();
        while let Some(batch) = stream
            .try_next()
            .await
            .context("stream historical coverage")?
        {
            let starts = int64(&batch, "start_ns")?;
            let ends = int64(&batch, "end_ns")?;
            covered.extend((0..batch.num_rows()).filter_map(|row| {
                TimeRange::new(
                    NanosecondTimestamp(starts.value(row)),
                    NanosecondTimestamp(ends.value(row)),
                )
                .ok()
            }));
        }
        Ok(Coverage { covered })
    }

    async fn read(&self, request: &HistoryRequest) -> Result<HistoricalData> {
        let table = Self::table(request)?;
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
            .client
            .query_stream(&sql)
            .await
            .with_context(|| format!("query cached history from {table}"))?;
        if request.configuration.option_chain.is_some() {
            let mut rows = Vec::new();
            while let Some(batch) = stream
                .try_next()
                .await
                .context("stream cached option universe")?
            {
                rows.extend(decode_option_universe(&batch)?);
            }
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
        self.client
            .append_stream(table, stream::iter(batches.into_iter().map(Ok)), &key)
            .await
            .with_context(|| format!("append canonical history to {table}"))?;
        Ok(())
    }

    async fn mark_covered(&self, request: &HistoryRequest, provider: &str) -> Result<()> {
        let sid = i64::try_from(request.configuration.symbol.sid())
            .context("symbol SID exceeds the canonical signed 64-bit contract")?;
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
        let mut stream = self
            .client
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
        let mut stream = self
            .client
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
    let sid = i64::try_from(request.configuration.symbol.sid())
        .context("symbol SID exceeds the canonical signed 64-bit contract")?;
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
    let sid = i64::try_from(request.configuration.symbol.sid())
        .context("symbol SID exceeds the canonical signed 64-bit contract")?;
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
    let sid = i64::try_from(request.configuration.symbol.sid())
        .context("symbol SID exceeds the canonical signed 64-bit contract")?;
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
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let open = decimal(batch, "open")?;
    let high = decimal(batch, "high")?;
    let low = decimal(batch, "low")?;
    let close = decimal(batch, "close")?;
    let volume = decimal(batch, "volume")?;
    let period = int64(batch, "period_ns")?;
    Ok((0..batch.num_rows())
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

fn encode_ticks(request: &HistoryRequest, rows: &[Tick]) -> Result<RecordBatch> {
    let sid = i64::try_from(request.configuration.symbol.sid())
        .context("symbol SID exceeds the canonical signed 64-bit contract")?;
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
    use rlean_data::SubscriptionDataConfig;
    use rust_decimal_macros::dec;

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
}
