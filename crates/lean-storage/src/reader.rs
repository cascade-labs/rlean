use crate::schema::{FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow};
use crate::{convert, predicate::Predicate, schema};
use arrow_array::{Float64Array, Int64Array, StringArray};
use datafusion::prelude::*;
use lean_core::{DateTime, Result as LeanResult, Symbol};
use lean_data::CustomDataPoint;
use lean_data::TradeBar;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
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
        let config = SessionConfig::new()
            .with_batch_size(65_536)
            .with_repartition_joins(true)
            .with_target_partitions(num_cpus::get());

        ParquetReader {
            ctx: SessionContext::new_with_config(config),
        }
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

impl Default for ParquetReader {
    fn default() -> Self {
        ParquetReader::new()
    }
}
