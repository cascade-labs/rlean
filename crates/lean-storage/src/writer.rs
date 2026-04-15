use crate::{convert, path_resolver::DataPath, schema};
use lean_data::{CustomDataPoint, QuoteBar, Tick, TradeBar};
use lean_core::Result as LeanResult;
use crate::schema::{FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow};
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, ZstdLevel},
    file::properties::WriterProperties,
};
use arrow_array::{Float64Array, Int64Array, StringArray, RecordBatch};
use std::{fs, path::Path, sync::Arc};
use tracing::{debug, info};

/// Configuration for Parquet output.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Zstandard compression level (1–22). Default 3.
    pub compression_level: i32,
    /// Row group size in number of rows. Default 128k.
    pub row_group_size: usize,
    /// Write statistics (min/max per column) for predicate pushdown.
    pub write_statistics: bool,
    /// Write bloom filters for high-cardinality columns.
    pub bloom_filter: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig {
            compression_level: 3,
            row_group_size: 131_072,
            write_statistics: true,
            bloom_filter: true,
        }
    }
}

/// Writes Parquet files for LEAN market data types.
pub struct ParquetWriter {
    config: WriterConfig,
}

impl ParquetWriter {
    pub fn new(config: WriterConfig) -> Self {
        ParquetWriter { config }
    }

    fn writer_props(&self) -> WriterProperties {
        let zstd = ZstdLevel::try_new(self.config.compression_level)
            .unwrap_or(ZstdLevel::try_new(3).unwrap());

        let mut builder = WriterProperties::builder()
            .set_compression(Compression::ZSTD(zstd))
            .set_max_row_group_size(self.config.row_group_size)
            .set_statistics_enabled(if self.config.write_statistics {
                parquet::file::properties::EnabledStatistics::Page
            } else {
                parquet::file::properties::EnabledStatistics::None
            });

        if self.config.bloom_filter {
            builder = builder.set_bloom_filter_enabled(true);
        }

        builder.build()
    }

    /// Write trade bars to a parquet file at the given path.
    pub fn write_trade_bars(&self, bars: &[TradeBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let batch = convert::trade_bars_to_record_batch(bars);
        let schema = schema::trade_bar_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} trade bars to {}", bars.len(), path.display());
        Ok(())
    }

    /// Write quote bars to a parquet file at the given path.
    pub fn write_quote_bars(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let batch = convert::quote_bars_to_record_batch(bars);
        let schema = schema::quote_bar_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} quote bars to {}", bars.len(), path.display());
        Ok(())
    }

    /// Write ticks to a parquet file at the given path.
    pub fn write_ticks(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        if ticks.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let batch = convert::ticks_to_record_batch(ticks);
        let schema = schema::tick_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} ticks to {}", ticks.len(), path.display());
        Ok(())
    }

    /// Write option EOD bars to a parquet file at the given path.
    pub fn write_option_eod_bars(&self, rows: &[OptionEodBar], path: &Path) -> LeanResult<()> {
        if rows.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let batch = convert::option_eod_bars_to_record_batch(rows);
        let schema = schema::option_eod_bar_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} option EOD bars to {}", rows.len(), path.display());
        Ok(())
    }

    /// Write option universe rows to a parquet file at the given path.
    pub fn write_option_universe(&self, rows: &[OptionUniverseRow], path: &Path) -> LeanResult<()> {
        if rows.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let batch = convert::option_universe_rows_to_record_batch(rows);
        let schema = schema::option_universe_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} option universe rows to {}", rows.len(), path.display());
        Ok(())
    }

    /// Write using a `DataPath` (preferred — derives path from symbol/date/resolution).
    pub fn write_trade_bars_at(&self, bars: &[TradeBar], data_path: &DataPath) -> LeanResult<()> {
        self.write_trade_bars(bars, &data_path.to_path())
    }

    pub fn write_quote_bars_at(&self, bars: &[QuoteBar], data_path: &DataPath) -> LeanResult<()> {
        self.write_quote_bars(bars, &data_path.to_path())
    }

    pub fn write_ticks_at(&self, ticks: &[Tick], data_path: &DataPath) -> LeanResult<()> {
        self.write_ticks(ticks, &data_path.to_path())
    }

    pub fn write_option_eod_bars_at(&self, rows: &[OptionEodBar], data_path: &DataPath) -> LeanResult<()> {
        self.write_option_eod_bars(rows, &data_path.to_path())
    }

    pub fn write_option_universe_at(&self, rows: &[OptionUniverseRow], data_path: &DataPath) -> LeanResult<()> {
        self.write_option_universe(rows, &data_path.to_path())
    }

    /// Write factor file entries to a parquet file.
    ///
    /// Schema: `date_ns` (Int64 ns UTC), `price_factor` (Float64),
    ///         `split_factor` (Float64), `reference_price` (Float64).
    pub fn write_factor_file(&self, entries: &[FactorFileEntry], path: &Path) -> LeanResult<()> {
        if entries.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let schema = schema::factor_file_schema();

        let dates:  Vec<i64> = entries.iter().map(|e| e.date_ns()).collect();
        let prices: Vec<f64> = entries.iter().map(|e| e.price_factor).collect();
        let splits: Vec<f64> = entries.iter().map(|e| e.split_factor).collect();
        let refs:   Vec<f64> = entries.iter().map(|e| e.reference_price).collect();

        let batch = RecordBatch::try_new(schema.clone(), vec![
            Arc::new(Int64Array::from(dates)),
            Arc::new(Float64Array::from(prices)),
            Arc::new(Float64Array::from(splits)),
            Arc::new(Float64Array::from(refs)),
        ]).map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} factor file entries to {}", entries.len(), path.display());
        Ok(())
    }

    /// Write map file entries to a parquet file.
    ///
    /// Schema: `date_ns` (Int64 ns UTC), `ticker` (Utf8).
    pub fn write_map_file(&self, entries: &[MapFileEntry], path: &Path) -> LeanResult<()> {
        if entries.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let schema = schema::map_file_schema();

        let dates:   Vec<i64>   = entries.iter().map(|e| e.date_ns()).collect();
        let tickers: Vec<&str>  = entries.iter().map(|e| e.ticker.as_str()).collect();

        let batch = RecordBatch::try_new(schema.clone(), vec![
            Arc::new(Int64Array::from(dates)),
            Arc::new(StringArray::from(tickers)),
        ]).map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} map file entries to {}", entries.len(), path.display());
        Ok(())
    }

    /// Write custom data points to a parquet cache file.
    ///
    /// Schema: `date_ns` (Int64 ns UTC), `value` (Float64), `fields_json` (Utf8).
    pub fn write_custom_data_points(&self, points: &[CustomDataPoint], path: &Path) -> LeanResult<()> {
        if points.is_empty() { return Ok(()); }
        self.ensure_dir(path)?;

        let schema = schema::custom_data_schema();

        let dates: Vec<i64> = points.iter().map(|p| schema::date_to_ns(p.time)).collect();
        let values: Vec<f64> = points.iter().map(|p| {
            use rust_decimal::prelude::ToPrimitive;
            p.value.to_f64().unwrap_or(0.0)
        }).collect();
        let fields_json: Vec<String> = points.iter()
            .map(|p| serde_json::to_string(&p.fields).unwrap_or_else(|_| "{}".to_string()))
            .collect();
        let fields_json_refs: Vec<&str> = fields_json.iter().map(|s| s.as_str()).collect();

        let batch = RecordBatch::try_new(schema.clone(), vec![
            Arc::new(Int64Array::from(dates)),
            Arc::new(arrow_array::Float64Array::from(values)),
            Arc::new(StringArray::from(fields_json_refs)),
        ]).map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer.close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} custom data points to {}", points.len(), path.display());
        Ok(())
    }

    fn ensure_dir(&self, path: &Path) -> LeanResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}
