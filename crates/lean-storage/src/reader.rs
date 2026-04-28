use crate::schema::{i64_to_price, FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow};
use crate::{convert, predicate::Predicate, schema};
use arrow_array::types::{
    TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType,
};
use arrow_array::{
    Array, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray, UInt32Array, UInt64Array,
};
use arrow_cast::display::array_value_to_string;
use chrono::{NaiveDate, TimeZone, Utc};
use datafusion::common::config::ConfigOptions;
use datafusion::prelude::*;
use lean_core::{DateTime, NanosecondTimestamp, Result as LeanResult, SecurityType, Symbol};
use lean_data::{
    Bar, CustomDataPoint, CustomDataQuery, CustomParquetSource, QuoteBar, Tick, TradeBar,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::debug;

/// Parameters for a data query.
#[derive(Debug, Clone, Default)]
pub struct QueryParams {
    pub predicate: Predicate,
    /// Maximum rows to return. None = unlimited.
    pub limit: Option<usize>,
    /// Sort ascending by time. Default true.
    pub order_by_time: bool,
}

impl QueryParams {
    pub fn new() -> Self {
        QueryParams {
            predicate: Predicate::new(),
            limit: None,
            order_by_time: true,
        }
    }

    pub fn with_time_range(mut self, start: DateTime, end: DateTime) -> Self {
        self.predicate = self.predicate.with_time_range(start, end);
        self
    }

    pub fn with_symbols(mut self, sids: Vec<u64>) -> Self {
        self.predicate = self.predicate.with_symbols(sids);
        self
    }

    pub fn with_limit(mut self, n: usize) -> Self {
        self.limit = Some(n);
        self
    }
}

/// Parquet reader with predicate pushdown via DataFusion.
pub struct ParquetReader {
    ctx: SessionContext,
}

impl ParquetReader {
    pub fn new() -> Self {
        let mut config_options = ConfigOptions::new();
        config_options.execution.parquet.pushdown_filters = true;
        config_options.execution.parquet.reorder_filters = true;

        let config = SessionConfig::from(config_options)
            .with_batch_size(65_536)
            .with_repartition_joins(true)
            .with_target_partitions(num_cpus::get());

        ParquetReader {
            ctx: SessionContext::new_with_config(config),
        }
    }

    /// Read custom data directly from provider-native parquet with generic
    /// projection and predicate pushdown through DataFusion.
    pub async fn read_custom_parquet_points(
        &self,
        source: &CustomParquetSource,
        query: &CustomDataQuery,
        date: chrono::NaiveDate,
    ) -> LeanResult<Vec<CustomDataPoint>> {
        validate_custom_parquet_source(source)?;
        if source.paths.is_empty() {
            return Ok(vec![]);
        }

        let table_name = format!(
            "custom_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );

        if source.paths.len() == 1 {
            self.ctx
                .register_parquet(&table_name, &source.paths[0], ParquetReadOptions::default())
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        } else {
            let first = Path::new(&source.paths[0]);
            let parent = first.parent().ok_or_else(|| {
                lean_core::LeanError::DataError(format!(
                    "parquet path has no parent: {}",
                    source.paths[0]
                ))
            })?;
            let listing_opts = datafusion::datasource::listing::ListingOptions::new(Arc::new(
                datafusion::datasource::file_format::parquet::ParquetFormat::new(),
            ))
            .with_file_extension(".parquet");
            let listing_table_url =
                datafusion::datasource::listing::ListingTableUrl::parse(parent.to_str().unwrap())
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            let listing_config =
                datafusion::datasource::listing::ListingTableConfig::new(listing_table_url)
                    .with_listing_options(listing_opts)
                    .infer_schema(&self.ctx.state())
                    .await
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            let listing_table =
                datafusion::datasource::listing::ListingTable::try_new(listing_config)
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            self.ctx
                .register_table(&table_name, Arc::new(listing_table))
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        let mut df = self
            .ctx
            .table(&table_name)
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let mut filters: Vec<Expr> = Vec::new();
        if let (Some(symbol_col), Some(symbols)) = (&source.symbol_column, &query.symbols) {
            if symbols.len() == 1 {
                filters.push(col(symbol_col).eq(lit(symbols[0].clone())));
            } else if !symbols.is_empty() {
                let expr = symbols
                    .iter()
                    .map(|s| col(symbol_col).eq(lit(s.clone())))
                    .reduce(|a, b| a.or(b))
                    .unwrap();
                filters.push(expr);
            }
        }
        for (column, value) in &query.string_equals {
            filters.push(col(column).eq(lit(value.clone())));
        }
        for (column, values) in &query.string_in {
            if values.len() == 1 {
                filters.push(col(column).eq(lit(values[0].clone())));
            } else if !values.is_empty() {
                let expr = values
                    .iter()
                    .map(|s| col(column).eq(lit(s.clone())))
                    .reduce(|a, b| a.or(b))
                    .unwrap();
                filters.push(expr);
            }
        }
        for (column, value) in &query.numeric_min {
            filters.push(col(column).gt_eq(lit(*value)));
        }
        for (column, value) in &query.numeric_max {
            filters.push(col(column).lt_eq(lit(*value)));
        }
        if let Some(filter) = filters.into_iter().reduce(|a, b| a.and(b)) {
            df = df
                .filter(filter)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        if let Some(columns) = &query.columns {
            let mut projection = columns.clone();
            for required in [
                source.time_column.as_ref(),
                source.symbol_column.as_ref(),
                source.value_column.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                if !projection.iter().any(|c| c == required) {
                    projection.push(required.clone());
                }
            }
            let refs: Vec<&str> = projection.iter().map(String::as_str).collect();
            df = df
                .select_columns(&refs)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        let batches = df
            .collect()
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        let _ = self.ctx.deregister_table(&table_name);

        let mut out = Vec::new();
        for batch in &batches {
            let schema = batch.schema();
            let time_idx = source
                .time_column
                .as_ref()
                .and_then(|c| schema.index_of(c).ok());
            let value_idx = source
                .value_column
                .as_ref()
                .and_then(|c| schema.index_of(c).ok());
            for row in 0..batch.num_rows() {
                let point_end_time = match time_idx {
                    Some(idx) => Some(parquet_custom_time_to_datetime(
                        batch.column(idx).as_ref(),
                        row,
                        source.time_format.as_deref(),
                        source.time_zone.as_deref(),
                        date,
                    )?),
                    None => None,
                };
                let point_date = point_end_time
                    .map(|time| {
                        time.to_tz(custom_time_zone(source.time_zone.as_deref()))
                            .date_naive()
                    })
                    .unwrap_or(date);
                let value = value_idx
                    .and_then(|idx| numeric_cell_as_f64(batch.column(idx).as_ref(), row))
                    .and_then(rust_decimal::Decimal::from_f64_retain)
                    .unwrap_or(rust_decimal::Decimal::ZERO);
                let mut fields = HashMap::new();
                for (col_idx, field) in schema.fields().iter().enumerate() {
                    fields.insert(
                        field.name().clone(),
                        arrow_cell_to_json(batch.column(col_idx).as_ref(), row),
                    );
                }
                out.push(CustomDataPoint {
                    time: point_date,
                    end_time: point_end_time,
                    value,
                    fields,
                });
            }
        }

        debug!(
            "Read {} custom parquet points from {} file(s)",
            out.len(),
            source.paths.len()
        );
        Ok(out)
    }

    /// Read trade bars from one or more parquet files with predicate pushdown.
    pub async fn read_trade_bars(
        &self,
        paths: &[PathBuf],
        symbol: Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<TradeBar>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        let table_name = format!("t_{}", symbol.id.sid);

        if paths.len() == 1 {
            let options = ParquetReadOptions::default();

            self.ctx
                .register_parquet(&table_name, paths[0].to_str().unwrap(), options)
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        } else {
            // Register as a listing table over multiple files.
            let listing_opts = datafusion::datasource::listing::ListingOptions::new(Arc::new(
                datafusion::datasource::file_format::parquet::ParquetFormat::new(),
            ))
            .with_file_extension(".parquet");

            let listing_table_url = datafusion::datasource::listing::ListingTableUrl::parse(
                paths[0].parent().unwrap().to_str().unwrap(),
            )
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            // Infer schema from first file
            let listing_config =
                datafusion::datasource::listing::ListingTableConfig::new(listing_table_url)
                    .with_listing_options(listing_opts)
                    .with_schema(schema::trade_bar_schema());

            let listing_table =
                datafusion::datasource::listing::ListingTable::try_new(listing_config)
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            self.ctx
                .register_table(&table_name, Arc::new(listing_table))
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        let mut df = self
            .ctx
            .table(&table_name)
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        // Apply predicate pushdown
        if let Some(filter) = params.predicate.to_datafusion_expr() {
            df = df
                .filter(filter)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        // Sort by time
        if params.order_by_time {
            df = df
                .sort(vec![col("time_ns").sort(true, true)])
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        // Apply limit
        if let Some(limit) = params.limit {
            df = df
                .limit(0, Some(limit))
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        }

        let batches = df
            .collect()
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        // Deregister to free memory
        let _ = self.ctx.deregister_table(&table_name);

        let mut result: Vec<TradeBar> = vec![];
        for batch in &batches {
            result.extend(convert::record_batch_to_trade_bars(batch, symbol.clone()));
        }

        debug!("Read {} trade bars (predicate applied)", result.len());
        Ok(result)
    }

    /// Read all parquet files matching a glob pattern (e.g., all dates for a symbol).
    pub async fn read_trade_bars_glob(
        &self,
        glob_pattern: &str,
        symbol: Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<TradeBar>> {
        let paths: Vec<PathBuf> = glob::glob(glob_pattern)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
            .filter_map(|r| r.ok())
            .collect();

        self.read_trade_bars(&paths, symbol, params).await
    }

    /// Read quote bars from one or more parquet files.
    pub fn read_quote_bars(
        &self,
        paths: &[PathBuf],
        symbol: Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<QuoteBar>> {
        self.read_quote_bars_with_symbols(
            paths,
            &HashMap::from([(symbol.value.clone(), symbol)]),
            params,
        )
    }

    /// Read ticks from one or more parquet files.
    pub fn read_ticks(
        &self,
        paths: &[PathBuf],
        symbol: Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<Tick>> {
        self.read_ticks_with_symbols(
            paths,
            &HashMap::from([(symbol.value.clone(), symbol)]),
            params,
        )
    }

    /// Read trade bars from files that may contain multiple symbols.
    ///
    /// Used for option-underlying intraday files where all contracts for an
    /// underlying are stored together and resolved via `symbol_value`.
    pub fn read_trade_bars_with_symbols(
        &self,
        paths: &[PathBuf],
        symbols_by_value: &HashMap<String, Symbol>,
        params: &QueryParams,
    ) -> LeanResult<Vec<TradeBar>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        let mut result = Vec::new();

        for path in paths {
            let file = std::fs::File::open(path).map_err(|e| {
                lean_core::LeanError::DataError(format!("{}: {}", path.display(), e))
            })?;

            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
                .build()
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            for batch_result in reader {
                let batch =
                    batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

                let symbol_values = batch
                    .column(3)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| {
                        lean_core::LeanError::DataError("symbol_value column missing".into())
                    })?;
                let time_ns = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or_else(|| {
                        lean_core::LeanError::DataError("time_ns column missing".into())
                    })?;

                for row_idx in 0..batch.num_rows() {
                    let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
                    if !matches_time_filter(time, params) {
                        continue;
                    }
                    let Some(symbol) = symbols_by_value.get(symbol_values.value(row_idx)) else {
                        continue;
                    };
                    let single = batch.slice(row_idx, 1);
                    result.extend(convert::record_batch_to_trade_bars(&single, symbol.clone()));
                }
            }
        }

        Ok(result)
    }

    /// Read every trade bar in an all-symbol partition, reconstructing symbols
    /// from each row's `symbol_value` using `template` for market/security type.
    pub fn read_trade_bar_partition(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<TradeBar>> {
        Ok(self
            .read_trade_bar_partition_grouped(path, template, params)?
            .into_values()
            .flatten()
            .collect())
    }

    /// Read every trade bar in an all-symbol partition using DataFusion
    /// projection/filter pushdown, grouped by stored SID.
    pub async fn read_trade_bar_partition_grouped_async(
        &self,
        path: &Path,
        symbols_by_sid: &HashMap<u64, Symbol>,
        params: &QueryParams,
    ) -> LeanResult<HashMap<u64, Vec<TradeBar>>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let batches = self
            .collect_market_partition_batches(
                path,
                params,
                &[
                    "time_ns",
                    "end_time_ns",
                    "symbol_sid",
                    "open",
                    "high",
                    "low",
                    "close",
                    "volume",
                    "period_ns",
                ],
            )
            .await?;
        trade_batches_to_grouped(&batches, symbols_by_sid, params)
    }

    /// Read every trade bar in an all-symbol partition, grouped by stored SID.
    pub fn read_trade_bar_partition_grouped(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<HashMap<u64, Vec<TradeBar>>> {
        let mut grouped: HashMap<u64, Vec<TradeBar>> = HashMap::new();
        let mut symbols: HashMap<u64, Symbol> = HashMap::new();
        if !path.exists() {
            return Ok(grouped);
        }

        for batch in record_batches(path)? {
            let symbol_sids = uint64_column(&batch, 2, "symbol_sid")?;
            let symbol_values = string_column(&batch, 3, "symbol_value")?;
            let time_ns = int64_column(&batch, 0, "time_ns")?;
            let end_ns = int64_column(&batch, 1, "end_time_ns")?;
            let open_col = int64_column(&batch, 4, "open")?;
            let high_col = int64_column(&batch, 5, "high")?;
            let low_col = int64_column(&batch, 6, "low")?;
            let close_col = int64_column(&batch, 7, "close")?;
            let volume_col = int64_column(&batch, 8, "volume")?;
            let period_col = int64_column(&batch, 9, "period_ns")?;

            for row_idx in 0..batch.num_rows() {
                let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
                if !matches_time_filter(time, params) {
                    continue;
                }
                let symbol_sid = symbol_sids.value(row_idx);
                if !matches_symbol_filter(symbol_sid, params) {
                    continue;
                }
                let symbol = symbols
                    .entry(symbol_sid)
                    .or_insert_with(|| {
                        symbol_for_partition_row(template, symbol_values.value(row_idx), symbol_sid)
                    })
                    .clone();
                grouped.entry(symbol_sid).or_default().push(TradeBar {
                    symbol,
                    time,
                    end_time: lean_core::NanosecondTimestamp(end_ns.value(row_idx)),
                    open: i64_to_price(open_col.value(row_idx)),
                    high: i64_to_price(high_col.value(row_idx)),
                    low: i64_to_price(low_col.value(row_idx)),
                    close: i64_to_price(close_col.value(row_idx)),
                    volume: i64_to_price(volume_col.value(row_idx)),
                    period: lean_core::TimeSpan::from_nanos(period_col.value(row_idx)),
                });
            }
        }

        Ok(grouped)
    }

    /// Read quote bars from files that may contain multiple symbols.
    pub fn read_quote_bars_with_symbols(
        &self,
        paths: &[PathBuf],
        symbols_by_value: &HashMap<String, Symbol>,
        params: &QueryParams,
    ) -> LeanResult<Vec<QuoteBar>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        let mut result = Vec::new();

        for path in paths {
            let file = std::fs::File::open(path).map_err(|e| {
                lean_core::LeanError::DataError(format!("{}: {}", path.display(), e))
            })?;

            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
                .build()
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            for batch_result in reader {
                let batch =
                    batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

                let symbol_values = batch
                    .column(3)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| {
                        lean_core::LeanError::DataError("symbol_value column missing".into())
                    })?;
                let time_ns = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or_else(|| {
                        lean_core::LeanError::DataError("time_ns column missing".into())
                    })?;

                for row_idx in 0..batch.num_rows() {
                    let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
                    if !matches_time_filter(time, params) {
                        continue;
                    }
                    let Some(symbol) = symbols_by_value.get(symbol_values.value(row_idx)) else {
                        continue;
                    };
                    let single = batch.slice(row_idx, 1);
                    result.extend(convert::record_batch_to_quote_bars(&single, symbol.clone()));
                }
            }
        }

        Ok(result)
    }

    /// Read every quote bar in an all-symbol partition.
    pub fn read_quote_bar_partition(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<QuoteBar>> {
        Ok(self
            .read_quote_bar_partition_grouped(path, template, params)?
            .into_values()
            .flatten()
            .collect())
    }

    /// Read every quote bar in an all-symbol partition using DataFusion
    /// projection/filter pushdown, grouped by stored SID.
    pub async fn read_quote_bar_partition_grouped_async(
        &self,
        path: &Path,
        symbols_by_sid: &HashMap<u64, Symbol>,
        params: &QueryParams,
    ) -> LeanResult<HashMap<u64, Vec<QuoteBar>>> {
        if !path.exists() {
            return Ok(HashMap::new());
        }

        let batches = self
            .collect_market_partition_batches(
                path,
                params,
                &[
                    "time_ns",
                    "end_time_ns",
                    "symbol_sid",
                    "bid_open",
                    "bid_high",
                    "bid_low",
                    "bid_close",
                    "ask_open",
                    "ask_high",
                    "ask_low",
                    "ask_close",
                    "last_bid_size",
                    "last_ask_size",
                    "period_ns",
                ],
            )
            .await?;
        quote_batches_to_grouped(&batches, symbols_by_sid, params)
    }

    /// Read every quote bar in an all-symbol partition, grouped by stored SID.
    pub fn read_quote_bar_partition_grouped(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<HashMap<u64, Vec<QuoteBar>>> {
        let mut grouped: HashMap<u64, Vec<QuoteBar>> = HashMap::new();
        let mut symbols: HashMap<u64, Symbol> = HashMap::new();
        if !path.exists() {
            return Ok(grouped);
        }

        for batch in record_batches(path)? {
            let symbol_sids = uint64_column(&batch, 2, "symbol_sid")?;
            let symbol_values = string_column(&batch, 3, "symbol_value")?;
            let time_ns = int64_column(&batch, 0, "time_ns")?;
            let end_ns = int64_column(&batch, 1, "end_time_ns")?;
            let bid_open_col = int64_column(&batch, 4, "bid_open")?;
            let bid_high_col = int64_column(&batch, 5, "bid_high")?;
            let bid_low_col = int64_column(&batch, 6, "bid_low")?;
            let bid_close_col = int64_column(&batch, 7, "bid_close")?;
            let ask_open_col = int64_column(&batch, 8, "ask_open")?;
            let ask_high_col = int64_column(&batch, 9, "ask_high")?;
            let ask_low_col = int64_column(&batch, 10, "ask_low")?;
            let ask_close_col = int64_column(&batch, 11, "ask_close")?;
            let last_bid_size_col = int64_column(&batch, 12, "last_bid_size")?;
            let last_ask_size_col = int64_column(&batch, 13, "last_ask_size")?;
            let period_col = int64_column(&batch, 14, "period_ns")?;

            for row_idx in 0..batch.num_rows() {
                let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
                if !matches_time_filter(time, params) {
                    continue;
                }
                let symbol_sid = symbol_sids.value(row_idx);
                if !matches_symbol_filter(symbol_sid, params) {
                    continue;
                }
                let symbol = symbols
                    .entry(symbol_sid)
                    .or_insert_with(|| {
                        symbol_for_partition_row(template, symbol_values.value(row_idx), symbol_sid)
                    })
                    .clone();
                let bid = if bid_open_col.is_null(row_idx) {
                    None
                } else {
                    Some(Bar {
                        open: i64_to_price(bid_open_col.value(row_idx)),
                        high: i64_to_price(bid_high_col.value(row_idx)),
                        low: i64_to_price(bid_low_col.value(row_idx)),
                        close: i64_to_price(bid_close_col.value(row_idx)),
                    })
                };
                let ask = if ask_open_col.is_null(row_idx) {
                    None
                } else {
                    Some(Bar {
                        open: i64_to_price(ask_open_col.value(row_idx)),
                        high: i64_to_price(ask_high_col.value(row_idx)),
                        low: i64_to_price(ask_low_col.value(row_idx)),
                        close: i64_to_price(ask_close_col.value(row_idx)),
                    })
                };
                grouped.entry(symbol_sid).or_default().push(QuoteBar {
                    symbol,
                    time,
                    end_time: lean_core::NanosecondTimestamp(end_ns.value(row_idx)),
                    bid,
                    ask,
                    last_bid_size: i64_to_price(last_bid_size_col.value(row_idx)),
                    last_ask_size: i64_to_price(last_ask_size_col.value(row_idx)),
                    period: lean_core::TimeSpan::from_nanos(period_col.value(row_idx)),
                });
            }
        }

        Ok(grouped)
    }

    async fn collect_market_partition_batches(
        &self,
        path: &Path,
        params: &QueryParams,
        columns: &[&str],
    ) -> LeanResult<Vec<RecordBatch>> {
        let table_name = format!(
            "market_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        );

        self.ctx
            .register_parquet(
                &table_name,
                path.to_str().unwrap(),
                ParquetReadOptions::default(),
            )
            .await
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let result = async {
            let mut df = self
                .ctx
                .table(&table_name)
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
                .select_columns(columns)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            if let Some(filter) = params.predicate.to_datafusion_expr() {
                df = df
                    .filter(filter)
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            }

            if let Some(limit) = params.limit {
                df = df
                    .limit(0, Some(limit))
                    .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            }

            df.collect()
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))
        }
        .await;

        let _ = self.ctx.deregister_table(&table_name);
        result
    }

    /// Read ticks from files that may contain multiple symbols.
    pub fn read_ticks_with_symbols(
        &self,
        paths: &[PathBuf],
        symbols_by_value: &HashMap<String, Symbol>,
        params: &QueryParams,
    ) -> LeanResult<Vec<Tick>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        let mut result = Vec::new();

        for path in paths {
            let file = std::fs::File::open(path).map_err(|e| {
                lean_core::LeanError::DataError(format!("{}: {}", path.display(), e))
            })?;

            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
                .build()
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            for batch_result in reader {
                let batch =
                    batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

                let symbol_values = batch
                    .column(2)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| {
                        lean_core::LeanError::DataError("symbol_value column missing".into())
                    })?;
                let time_ns = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<Int64Array>()
                    .ok_or_else(|| {
                        lean_core::LeanError::DataError("time_ns column missing".into())
                    })?;

                for row_idx in 0..batch.num_rows() {
                    let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
                    if !matches_time_filter(time, params) {
                        continue;
                    }
                    let Some(symbol) = symbols_by_value.get(symbol_values.value(row_idx)) else {
                        continue;
                    };
                    let single = batch.slice(row_idx, 1);
                    result.extend(convert::record_batch_to_ticks(&single, symbol.clone()));
                }
            }
        }

        Ok(result)
    }

    /// Read every tick in an all-symbol partition.
    pub fn read_tick_partition(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<Tick>> {
        let mut result = Vec::new();
        if !path.exists() {
            return Ok(result);
        }

        for batch in record_batches(path)? {
            let symbol_sids = uint64_column(&batch, 1, "symbol_sid")?;
            let symbol_values = string_column(&batch, 2, "symbol_value")?;
            let time_ns = int64_column(&batch, 0, "time_ns")?;

            for row_idx in 0..batch.num_rows() {
                let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
                if !matches_time_filter(time, params) {
                    continue;
                }
                let symbol_sid = symbol_sids.value(row_idx);
                if !matches_symbol_filter(symbol_sid, params) {
                    continue;
                }
                let symbol =
                    symbol_like_with_sid(template, symbol_values.value(row_idx), symbol_sid);
                result.extend(convert::record_batch_to_ticks(
                    &batch.slice(row_idx, 1),
                    symbol,
                ));
            }
        }

        Ok(result)
    }

    /// Return all symbol SIDs present in a market-data partition.
    pub fn read_partition_symbol_sids(
        &self,
        path: &Path,
        symbol_sid_column: usize,
    ) -> LeanResult<HashSet<u64>> {
        let mut result = HashSet::new();
        if !path.exists() {
            return Ok(result);
        }

        for batch in record_batches(path)? {
            let symbol_sids = uint64_column(&batch, symbol_sid_column, "symbol_sid")?;
            for row_idx in 0..batch.num_rows() {
                result.insert(symbol_sids.value(row_idx));
            }
        }

        Ok(result)
    }

    /// Read option EOD bars from one or more parquet files.
    ///
    /// Each file typically covers all contracts for one underlying.  The rows
    /// from all provided files are concatenated and returned unsorted.
    pub fn read_option_eod_bars(&self, paths: &[PathBuf]) -> LeanResult<Vec<OptionEodBar>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        let mut result: Vec<OptionEodBar> = Vec::new();

        for path in paths {
            let file = std::fs::File::open(path).map_err(|e| {
                lean_core::LeanError::DataError(format!("{}: {}", path.display(), e))
            })?;

            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
                .build()
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            for batch_result in reader {
                let batch =
                    batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
                result.extend(convert::record_batch_to_option_eod_bars(&batch));
            }
        }

        debug!(
            "Read {} option EOD bars from {} file(s)",
            result.len(),
            paths.len()
        );
        Ok(result)
    }

    /// Read option universe rows from one or more parquet files.
    pub fn read_option_universe(&self, paths: &[PathBuf]) -> LeanResult<Vec<OptionUniverseRow>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        let mut result: Vec<OptionUniverseRow> = Vec::new();

        for path in paths {
            let file = std::fs::File::open(path).map_err(|e| {
                lean_core::LeanError::DataError(format!("{}: {}", path.display(), e))
            })?;

            let reader = ParquetRecordBatchReaderBuilder::try_new(file)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
                .build()
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            for batch_result in reader {
                let batch =
                    batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
                result.extend(convert::record_batch_to_option_universe_rows(&batch));
            }
        }

        debug!(
            "Read {} option universe rows from {} file(s)",
            result.len(),
            paths.len()
        );
        Ok(result)
    }

    /// Read factor file entries from a parquet file.
    ///
    /// Returns an empty `Vec` (no error) when the file does not exist.
    /// Schema: `date_ns` (Int64 ns UTC), `price_factor` (Float64),
    ///         `split_factor` (Float64), `reference_price` (Float64).
    pub fn read_factor_file(&self, path: &Path) -> LeanResult<Vec<FactorFileEntry>> {
        if !path.exists() {
            return Ok(vec![]);
        }

        let file = std::fs::File::open(path)
            .map_err(|e| lean_core::LeanError::DataError(format!("{}: {}", path.display(), e)))?;

        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
            .build()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let mut result = Vec::new();
        for batch_result in reader {
            let batch = batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            let dates = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| lean_core::LeanError::DataError("date_ns column missing".into()))?;
            let prices = batch
                .column(1)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| {
                    lean_core::LeanError::DataError("price_factor column missing".into())
                })?;
            let splits = batch
                .column(2)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| {
                    lean_core::LeanError::DataError("split_factor column missing".into())
                })?;
            let refs = batch
                .column(3)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| {
                    lean_core::LeanError::DataError("reference_price column missing".into())
                })?;

            for i in 0..batch.num_rows() {
                result.push(FactorFileEntry {
                    date: schema::ns_to_date(dates.value(i)),
                    price_factor: prices.value(i),
                    split_factor: splits.value(i),
                    reference_price: refs.value(i),
                });
            }
        }

        debug!(
            "Read {} factor file entries from {}",
            result.len(),
            path.display()
        );
        Ok(result)
    }

    /// Read custom data points from a parquet cache file.
    ///
    /// Returns an empty `Vec` (no error) when the file does not exist.
    /// Schema: `date_ns` (Int64 ns UTC), `value` (Float64), `fields_json` (Utf8).
    pub fn read_custom_data_points(&self, path: &Path) -> LeanResult<Vec<CustomDataPoint>> {
        if !path.exists() {
            return Ok(vec![]);
        }

        let file = std::fs::File::open(path)
            .map_err(|e| lean_core::LeanError::DataError(format!("{}: {}", path.display(), e)))?;

        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
            .build()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let mut result = Vec::new();
        for batch_result in reader {
            let batch = batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            let dates = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| lean_core::LeanError::DataError("date_ns column missing".into()))?;
            let values = batch
                .column(1)
                .as_any()
                .downcast_ref::<Float64Array>()
                .ok_or_else(|| lean_core::LeanError::DataError("value column missing".into()))?;
            let fields_col = batch
                .column(2)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| {
                    lean_core::LeanError::DataError("fields_json column missing".into())
                })?;

            for i in 0..batch.num_rows() {
                let date = schema::ns_to_date(dates.value(i));
                let value_f64 = values.value(i);
                let value = rust_decimal::Decimal::from_f64_retain(value_f64)
                    .unwrap_or(rust_decimal::Decimal::ZERO);
                let fields: std::collections::HashMap<String, serde_json::Value> =
                    serde_json::from_str(fields_col.value(i)).unwrap_or_default();
                result.push(CustomDataPoint {
                    time: date,
                    end_time: Some(NanosecondTimestamp(dates.value(i))),
                    value,
                    fields,
                });
            }
        }

        debug!(
            "Read {} custom data points from {}",
            result.len(),
            path.display()
        );
        Ok(result)
    }

    /// Read map file entries from a parquet file.
    ///
    /// Returns an empty `Vec` (no error) when the file does not exist.
    /// Schema: `date_ns` (Int64 ns UTC), `ticker` (Utf8).
    pub fn read_map_file(&self, path: &Path) -> LeanResult<Vec<MapFileEntry>> {
        if !path.exists() {
            return Ok(vec![]);
        }

        let file = std::fs::File::open(path)
            .map_err(|e| lean_core::LeanError::DataError(format!("{}: {}", path.display(), e)))?;

        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
            .build()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let mut result = Vec::new();
        for batch_result in reader {
            let batch = batch_result.map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            let dates = batch
                .column(0)
                .as_any()
                .downcast_ref::<Int64Array>()
                .ok_or_else(|| lean_core::LeanError::DataError("date_ns column missing".into()))?;
            let tickers = batch
                .column(1)
                .as_any()
                .downcast_ref::<StringArray>()
                .ok_or_else(|| lean_core::LeanError::DataError("ticker column missing".into()))?;

            for i in 0..batch.num_rows() {
                result.push(MapFileEntry {
                    date: schema::ns_to_date(dates.value(i)),
                    ticker: tickers.value(i).to_uppercase(),
                });
            }
        }

        debug!(
            "Read {} map file entries from {}",
            result.len(),
            path.display()
        );
        Ok(result)
    }
}

fn validate_custom_parquet_source(source: &CustomParquetSource) -> LeanResult<()> {
    match (&source.time_column, source.time_format.as_deref()) {
        (Some(_), Some("timestamp")) | (None, None) => Ok(()),
        (Some(_), None) => Err(lean_core::LeanError::DataError(
            "custom parquet source with time_column must set time_format='timestamp'".to_string(),
        )),
        (Some(_), Some(other)) => Err(lean_core::LeanError::DataError(format!(
            "unsupported custom parquet time_format '{other}'; native custom parquet requires Arrow timestamp columns"
        ))),
        (None, Some(_)) => Err(lean_core::LeanError::DataError(
            "custom parquet source cannot set time_format without time_column".to_string(),
        )),
    }
}

fn matches_time_filter(time: DateTime, params: &QueryParams) -> bool {
    if let Some(start) = params.predicate.start_time {
        if time.0 < start.0 {
            return false;
        }
    }
    if let Some(end) = params.predicate.end_time {
        if time.0 >= end.0 {
            return false;
        }
    }
    true
}

fn matches_symbol_filter(symbol_sid: u64, params: &QueryParams) -> bool {
    let Some(ref sids) = params.predicate.symbol_sids else {
        return true;
    };
    sids.contains(&symbol_sid)
}

fn trade_batches_to_grouped(
    batches: &[RecordBatch],
    symbols_by_sid: &HashMap<u64, Symbol>,
    params: &QueryParams,
) -> LeanResult<HashMap<u64, Vec<TradeBar>>> {
    let mut grouped: HashMap<u64, Vec<TradeBar>> = HashMap::new();

    for batch in batches {
        let symbol_sids = uint64_column_named(batch, "symbol_sid")?;
        let time_ns = int64_column_named(batch, "time_ns")?;
        let end_ns = int64_column_named(batch, "end_time_ns")?;
        let open_col = int64_column_named(batch, "open")?;
        let high_col = int64_column_named(batch, "high")?;
        let low_col = int64_column_named(batch, "low")?;
        let close_col = int64_column_named(batch, "close")?;
        let volume_col = int64_column_named(batch, "volume")?;
        let period_col = int64_column_named(batch, "period_ns")?;

        for row_idx in 0..batch.num_rows() {
            let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
            if !matches_time_filter(time, params) {
                continue;
            }
            let symbol_sid = symbol_sids.value(row_idx);
            if !matches_symbol_filter(symbol_sid, params) {
                continue;
            }
            let Some(symbol) = symbols_by_sid.get(&symbol_sid).cloned() else {
                continue;
            };
            grouped.entry(symbol_sid).or_default().push(TradeBar {
                symbol,
                time,
                end_time: lean_core::NanosecondTimestamp(end_ns.value(row_idx)),
                open: i64_to_price(open_col.value(row_idx)),
                high: i64_to_price(high_col.value(row_idx)),
                low: i64_to_price(low_col.value(row_idx)),
                close: i64_to_price(close_col.value(row_idx)),
                volume: i64_to_price(volume_col.value(row_idx)),
                period: lean_core::TimeSpan::from_nanos(period_col.value(row_idx)),
            });
        }
    }

    Ok(grouped)
}

fn quote_batches_to_grouped(
    batches: &[RecordBatch],
    symbols_by_sid: &HashMap<u64, Symbol>,
    params: &QueryParams,
) -> LeanResult<HashMap<u64, Vec<QuoteBar>>> {
    let mut grouped: HashMap<u64, Vec<QuoteBar>> = HashMap::new();

    for batch in batches {
        let symbol_sids = uint64_column_named(batch, "symbol_sid")?;
        let time_ns = int64_column_named(batch, "time_ns")?;
        let end_ns = int64_column_named(batch, "end_time_ns")?;
        let bid_open_col = int64_column_named(batch, "bid_open")?;
        let bid_high_col = int64_column_named(batch, "bid_high")?;
        let bid_low_col = int64_column_named(batch, "bid_low")?;
        let bid_close_col = int64_column_named(batch, "bid_close")?;
        let ask_open_col = int64_column_named(batch, "ask_open")?;
        let ask_high_col = int64_column_named(batch, "ask_high")?;
        let ask_low_col = int64_column_named(batch, "ask_low")?;
        let ask_close_col = int64_column_named(batch, "ask_close")?;
        let last_bid_size_col = int64_column_named(batch, "last_bid_size")?;
        let last_ask_size_col = int64_column_named(batch, "last_ask_size")?;
        let period_col = int64_column_named(batch, "period_ns")?;

        for row_idx in 0..batch.num_rows() {
            let time = lean_core::NanosecondTimestamp(time_ns.value(row_idx));
            if !matches_time_filter(time, params) {
                continue;
            }
            let symbol_sid = symbol_sids.value(row_idx);
            if !matches_symbol_filter(symbol_sid, params) {
                continue;
            }
            let Some(symbol) = symbols_by_sid.get(&symbol_sid).cloned() else {
                continue;
            };
            let bid = if bid_open_col.is_null(row_idx) {
                None
            } else {
                Some(Bar {
                    open: i64_to_price(bid_open_col.value(row_idx)),
                    high: i64_to_price(bid_high_col.value(row_idx)),
                    low: i64_to_price(bid_low_col.value(row_idx)),
                    close: i64_to_price(bid_close_col.value(row_idx)),
                })
            };
            let ask = if ask_open_col.is_null(row_idx) {
                None
            } else {
                Some(Bar {
                    open: i64_to_price(ask_open_col.value(row_idx)),
                    high: i64_to_price(ask_high_col.value(row_idx)),
                    low: i64_to_price(ask_low_col.value(row_idx)),
                    close: i64_to_price(ask_close_col.value(row_idx)),
                })
            };
            grouped.entry(symbol_sid).or_default().push(QuoteBar {
                symbol,
                time,
                end_time: lean_core::NanosecondTimestamp(end_ns.value(row_idx)),
                bid,
                ask,
                last_bid_size: i64_to_price(last_bid_size_col.value(row_idx)),
                last_ask_size: i64_to_price(last_ask_size_col.value(row_idx)),
                period: lean_core::TimeSpan::from_nanos(period_col.value(row_idx)),
            });
        }
    }

    Ok(grouped)
}

fn record_batches(path: &Path) -> LeanResult<Vec<arrow_array::RecordBatch>> {
    let file = std::fs::File::open(path)
        .map_err(|e| lean_core::LeanError::DataError(format!("{}: {}", path.display(), e)))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?
        .build()
        .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

    reader
        .map(|batch| batch.map_err(|e| lean_core::LeanError::DataError(e.to_string())))
        .collect()
}

fn string_column<'a>(
    batch: &'a arrow_array::RecordBatch,
    index: usize,
    name: &str,
) -> LeanResult<&'a StringArray> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| lean_core::LeanError::DataError(format!("{name} column missing")))
}

fn int64_column<'a>(
    batch: &'a arrow_array::RecordBatch,
    index: usize,
    name: &str,
) -> LeanResult<&'a Int64Array> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| lean_core::LeanError::DataError(format!("{name} column missing")))
}

fn uint64_column<'a>(
    batch: &'a arrow_array::RecordBatch,
    index: usize,
    name: &str,
) -> LeanResult<&'a UInt64Array> {
    batch
        .column(index)
        .as_any()
        .downcast_ref::<UInt64Array>()
        .ok_or_else(|| lean_core::LeanError::DataError(format!("{name} column missing")))
}

fn column_index(batch: &arrow_array::RecordBatch, name: &str) -> LeanResult<usize> {
    let schema = batch.schema();
    schema.index_of(name).map_err(|_| {
        let available = schema
            .fields()
            .iter()
            .map(|field| field.name().as_str())
            .collect::<Vec<_>>()
            .join(",");
        lean_core::LeanError::DataError(format!(
            "{name} column missing; available columns: {available}"
        ))
    })
}

fn int64_column_named<'a>(
    batch: &'a arrow_array::RecordBatch,
    name: &str,
) -> LeanResult<&'a Int64Array> {
    int64_column(batch, column_index(batch, name)?, name)
}

fn uint64_column_named<'a>(
    batch: &'a arrow_array::RecordBatch,
    name: &str,
) -> LeanResult<&'a UInt64Array> {
    uint64_column(batch, column_index(batch, name)?, name)
}

fn symbol_like(template: &Symbol, symbol_value: &str) -> Symbol {
    match template.security_type() {
        SecurityType::Equity | SecurityType::Index => {
            Symbol::create_equity(symbol_value, template.market())
        }
        SecurityType::Forex => Symbol::create_forex(symbol_value),
        SecurityType::Crypto => Symbol::create_crypto(symbol_value, template.market()),
        _ => {
            let mut symbol = template.clone();
            symbol.value = symbol_value.to_string();
            symbol.permtick = symbol_value.to_string();
            symbol
        }
    }
}

fn symbol_like_with_sid(template: &Symbol, symbol_value: &str, symbol_sid: u64) -> Symbol {
    let mut symbol = symbol_like(template, symbol_value);
    symbol.id.sid = symbol_sid;
    symbol
}

fn symbol_for_partition_row(template: &Symbol, symbol_value: &str, symbol_sid: u64) -> Symbol {
    if template.id.sid == symbol_sid {
        return template.clone();
    }
    symbol_like_with_sid(template, symbol_value, symbol_sid)
}

fn parquet_custom_time_to_datetime(
    array: &dyn Array,
    row: usize,
    format: Option<&str>,
    time_zone: Option<&str>,
    _date: NaiveDate,
) -> LeanResult<DateTime> {
    if array.is_null(row) {
        return Err(lean_core::LeanError::DataError(
            "custom parquet timestamp column contains null".to_string(),
        ));
    }

    let time = match format {
        Some("timestamp") => arrow_timestamp_cell_as_datetime(array, row, time_zone),
        Some(other) => {
            return Err(lean_core::LeanError::DataError(format!(
                "unsupported custom parquet time_format '{other}'; native custom parquet requires Arrow timestamp columns"
            )))
        }
        None => {
            return Err(lean_core::LeanError::DataError(
                "custom parquet source with time_column must set time_format='timestamp'"
                    .to_string(),
            ))
        }
    };
    time.ok_or_else(|| {
        lean_core::LeanError::DataError(
            "custom parquet timestamp column must be Arrow Timestamp".to_string(),
        )
    })
}

fn arrow_timestamp_cell_as_datetime(
    array: &dyn Array,
    row: usize,
    time_zone: Option<&str>,
) -> Option<DateTime> {
    let raw_ns = if let Some(values) = array
        .as_any()
        .downcast_ref::<arrow_array::PrimitiveArray<TimestampNanosecondType>>()
    {
        values.value(row)
    } else if let Some(values) = array
        .as_any()
        .downcast_ref::<arrow_array::PrimitiveArray<TimestampMicrosecondType>>()
    {
        values.value(row) * 1_000
    } else if let Some(values) = array
        .as_any()
        .downcast_ref::<arrow_array::PrimitiveArray<TimestampMillisecondType>>()
    {
        values.value(row) * 1_000_000
    } else if let Some(values) = array
        .as_any()
        .downcast_ref::<arrow_array::PrimitiveArray<TimestampSecondType>>()
    {
        values.value(row) * 1_000_000_000
    } else {
        return None;
    };

    let timestamp = NanosecondTimestamp(raw_ns);
    match time_zone {
        Some(tz) => {
            let local = timestamp.to_utc().naive_utc();
            let tz = custom_time_zone(Some(tz));
            let zoned = tz
                .from_local_datetime(&local)
                .single()
                .or_else(|| tz.from_local_datetime(&local).earliest())
                .or_else(|| tz.from_local_datetime(&local).latest())?;
            Some(DateTime::from(zoned.with_timezone(&Utc)))
        }
        None => Some(timestamp),
    }
}

fn custom_time_zone(time_zone: Option<&str>) -> chrono_tz::Tz {
    time_zone
        .and_then(|tz| tz.parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

fn numeric_cell_as_f64(array: &dyn Array, row: usize) -> Option<f64> {
    if array.is_null(row) {
        return None;
    }
    if let Some(arr) = array.as_any().downcast_ref::<Float64Array>() {
        Some(arr.value(row))
    } else if let Some(arr) = array.as_any().downcast_ref::<Float32Array>() {
        Some(arr.value(row) as f64)
    } else if let Some(arr) = array.as_any().downcast_ref::<Int64Array>() {
        Some(arr.value(row) as f64)
    } else if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
        Some(arr.value(row) as f64)
    } else if let Some(arr) = array.as_any().downcast_ref::<UInt64Array>() {
        Some(arr.value(row) as f64)
    } else {
        array.as_any().downcast_ref::<UInt32Array>().map_or_else(
            || {
                array_value_to_string(array, row)
                    .ok()
                    .and_then(|s| s.parse::<f64>().ok())
            },
            |arr| Some(arr.value(row) as f64),
        )
    }
}

fn arrow_cell_to_json(array: &dyn Array, row: usize) -> serde_json::Value {
    if array.is_null(row) {
        return serde_json::Value::Null;
    }
    if let Some(arr) = array.as_any().downcast_ref::<StringArray>() {
        serde_json::Value::String(arr.value(row).to_string())
    } else if let Some(arr) = array.as_any().downcast_ref::<Float64Array>() {
        serde_json::Number::from_f64(arr.value(row))
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    } else if let Some(arr) = array.as_any().downcast_ref::<Float32Array>() {
        serde_json::Number::from_f64(arr.value(row) as f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    } else if let Some(arr) = array.as_any().downcast_ref::<Int64Array>() {
        serde_json::Value::Number(arr.value(row).into())
    } else if let Some(arr) = array.as_any().downcast_ref::<Int32Array>() {
        serde_json::Value::Number(arr.value(row).into())
    } else if let Some(arr) = array.as_any().downcast_ref::<UInt64Array>() {
        serde_json::Value::Number(arr.value(row).into())
    } else if let Some(arr) = array.as_any().downcast_ref::<UInt32Array>() {
        serde_json::Value::Number(arr.value(row).into())
    } else if let Some(arr) = array.as_any().downcast_ref::<BooleanArray>() {
        serde_json::Value::Bool(arr.value(row))
    } else {
        match array_value_to_string(array, row) {
            Ok(s) => serde_json::Number::from_f64(s.parse::<f64>().unwrap_or(f64::NAN))
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::String(s)),
            Err(_) => serde_json::Value::Null,
        }
    }
}

impl Default for ParquetReader {
    fn default() -> Self {
        ParquetReader::new()
    }
}
