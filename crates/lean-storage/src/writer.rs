use crate::schema::{FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow};
use crate::{convert, schema};
use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
use fs2::FileExt;
use lean_core::Result as LeanResult;
use lean_data::{CustomDataPoint, QuoteBar, Tick, TradeBar};
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, ZstdLevel},
    file::properties::WriterProperties,
};
use std::{collections::HashSet, fs, path::Path, sync::Arc};
use tracing::debug;

/// Compression codec for locally cached Parquet.
///
/// Local cache files optimize repeated backtest read speed, not cold-storage
/// density. Snappy is the default because it is substantially cheaper to decode
/// than ZSTD for repeatedly scanned market data partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterCompression {
    Snappy,
    Zstd,
    Uncompressed,
}

/// Configuration for Parquet output.
#[derive(Debug, Clone)]
pub struct WriterConfig {
    /// Compression codec. Default Snappy for local-cache scan speed.
    pub compression: WriterCompression,
    /// Zstandard compression level (1–22). Only used with ZSTD. Default 1.
    pub compression_level: i32,
    /// Row group size in rows. Default 8k to make SID predicates skip chunks.
    pub row_group_size: usize,
    /// Write statistics (min/max per column) for predicate pushdown.
    pub write_statistics: bool,
    /// Write bloom filters for high-cardinality columns.
    pub bloom_filter: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        WriterConfig {
            compression: WriterCompression::Snappy,
            compression_level: 1,
            row_group_size: 8_192,
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
        let compression = match self.config.compression {
            WriterCompression::Snappy => Compression::SNAPPY,
            WriterCompression::Zstd => {
                let zstd = ZstdLevel::try_new(self.config.compression_level)
                    .unwrap_or(ZstdLevel::try_new(1).unwrap());
                Compression::ZSTD(zstd)
            }
            WriterCompression::Uncompressed => Compression::UNCOMPRESSED,
        };

        let mut builder = WriterProperties::builder()
            .set_compression(compression)
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
        if bars.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let batch = convert::trade_bars_to_record_batch(bars);
        let schema = schema::trade_bar_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} trade bars to {}", bars.len(), path.display());
        Ok(())
    }

    /// Write quote bars to a parquet file at the given path.
    pub fn write_quote_bars(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let batch = convert::quote_bars_to_record_batch(bars);
        let schema = schema::quote_bar_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} quote bars to {}", bars.len(), path.display());
        Ok(())
    }

    /// Write ticks to a parquet file at the given path.
    pub fn write_ticks(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        if ticks.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let batch = convert::ticks_to_record_batch(ticks);
        let schema = schema::tick_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} ticks to {}", ticks.len(), path.display());
        Ok(())
    }

    /// Merge trade bars into a daily all-symbol partition. Existing rows for
    /// symbols present in `bars` are replaced; all other symbols are preserved.
    pub fn merge_trade_bar_partition(&self, bars: &[TradeBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }

        let _lock = self.lock_partition(path)?;
        let replacement_symbols: HashSet<String> =
            bars.iter().map(|bar| bar.symbol.value.clone()).collect();
        if partition_has_all_trade_rows(path, bars)? {
            return Ok(());
        }
        let mut merged = if path.exists() {
            crate::reader::ParquetReader::new()
                .read_trade_bar_partition(path, &bars[0].symbol, &Default::default())?
                .into_iter()
                .filter(|bar| !replacement_symbols.contains(&bar.symbol.value))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        merged.extend_from_slice(bars);
        dedupe_trade_bars(&mut merged);
        sort_trade_bars_for_predicate_pruning(&mut merged);
        self.write_trade_bars_atomic(&merged, path)
    }

    /// Merge quote bars into a daily all-symbol partition. Existing rows for
    /// symbols present in `bars` are replaced; all other symbols are preserved.
    pub fn merge_quote_bar_partition(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }

        let _lock = self.lock_partition(path)?;
        let replacement_symbols: HashSet<String> =
            bars.iter().map(|bar| bar.symbol.value.clone()).collect();
        if partition_has_all_quote_rows(path, bars)? {
            return Ok(());
        }
        let mut merged = if path.exists() {
            crate::reader::ParquetReader::new()
                .read_quote_bar_partition(path, &bars[0].symbol, &Default::default())?
                .into_iter()
                .filter(|bar| !replacement_symbols.contains(&bar.symbol.value))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        merged.extend_from_slice(bars);
        dedupe_quote_bars(&mut merged);
        sort_quote_bars_for_predicate_pruning(&mut merged);
        self.write_quote_bars_atomic(&merged, path)
    }

    /// Merge ticks into a daily all-symbol partition. Existing rows for symbols
    /// present in `ticks` are replaced; all other symbols are preserved.
    pub fn merge_tick_partition(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        if ticks.is_empty() {
            return Ok(());
        }

        let _lock = self.lock_partition(path)?;
        let replacement_symbols: HashSet<String> =
            ticks.iter().map(|tick| tick.symbol.value.clone()).collect();
        if partition_has_all_tick_rows(path, ticks)? {
            return Ok(());
        }
        let mut merged = if path.exists() {
            crate::reader::ParquetReader::new()
                .read_tick_partition(path, &ticks[0].symbol, &Default::default())?
                .into_iter()
                .filter(|tick| !replacement_symbols.contains(&tick.symbol.value))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        merged.extend_from_slice(ticks);
        dedupe_ticks(&mut merged);
        sort_ticks_for_predicate_pruning(&mut merged);
        self.write_ticks_atomic(&merged, path)
    }

    /// Write option EOD bars to a parquet file at the given path.
    pub fn write_option_eod_bars(&self, rows: &[OptionEodBar], path: &Path) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let batch = convert::option_eod_bars_to_record_batch(rows);
        let schema = schema::option_eod_bar_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!("Wrote {} option EOD bars to {}", rows.len(), path.display());
        Ok(())
    }

    /// Write option universe rows to a parquet file at the given path.
    pub fn write_option_universe(&self, rows: &[OptionUniverseRow], path: &Path) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let batch = convert::option_universe_rows_to_record_batch(rows);
        let schema = schema::option_universe_schema();

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!(
            "Wrote {} option universe rows to {}",
            rows.len(),
            path.display()
        );
        Ok(())
    }

    /// Write factor file entries to a parquet file.
    ///
    /// Schema: `date_ns` (Int64 ns UTC), `price_factor` (Float64),
    ///         `split_factor` (Float64), `reference_price` (Float64).
    pub fn write_factor_file(&self, entries: &[FactorFileEntry], path: &Path) -> LeanResult<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let schema = schema::factor_file_schema();

        let dates: Vec<i64> = entries.iter().map(|e| e.date_ns()).collect();
        let prices: Vec<f64> = entries.iter().map(|e| e.price_factor).collect();
        let splits: Vec<f64> = entries.iter().map(|e| e.split_factor).collect();
        let refs: Vec<f64> = entries.iter().map(|e| e.reference_price).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(dates)),
                Arc::new(Float64Array::from(prices)),
                Arc::new(Float64Array::from(splits)),
                Arc::new(Float64Array::from(refs)),
            ],
        )
        .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!(
            "Wrote {} factor file entries to {}",
            entries.len(),
            path.display()
        );
        Ok(())
    }

    /// Write map file entries to a parquet file.
    ///
    /// Schema: `date_ns` (Int64 ns UTC), `ticker` (Utf8).
    pub fn write_map_file(&self, entries: &[MapFileEntry], path: &Path) -> LeanResult<()> {
        if entries.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let schema = schema::map_file_schema();

        let dates: Vec<i64> = entries.iter().map(|e| e.date_ns()).collect();
        let tickers: Vec<&str> = entries.iter().map(|e| e.ticker.as_str()).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(dates)),
                Arc::new(StringArray::from(tickers)),
            ],
        )
        .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!(
            "Wrote {} map file entries to {}",
            entries.len(),
            path.display()
        );
        Ok(())
    }

    /// Write custom data points to a parquet cache file.
    ///
    /// Schema: `date_ns` (Int64 ns UTC), `value` (Float64), `fields_json` (Utf8).
    pub fn write_custom_data_points(
        &self,
        points: &[CustomDataPoint],
        path: &Path,
    ) -> LeanResult<()> {
        if points.is_empty() {
            return Ok(());
        }
        self.ensure_dir(path)?;

        let schema = schema::custom_data_schema();

        let dates: Vec<i64> = points
            .iter()
            .map(|p| {
                p.end_time
                    .map(|t| t.0)
                    .unwrap_or_else(|| schema::date_to_ns(p.time))
            })
            .collect();
        let values: Vec<f64> = points
            .iter()
            .map(|p| {
                use rust_decimal::prelude::ToPrimitive;
                p.value.to_f64().unwrap_or(0.0)
            })
            .collect();
        let fields_json: Vec<String> = points
            .iter()
            .map(|p| serde_json::to_string(&p.fields).unwrap_or_else(|_| "{}".to_string()))
            .collect();
        let fields_json_refs: Vec<&str> = fields_json.iter().map(|s| s.as_str()).collect();

        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(dates)),
                Arc::new(arrow_array::Float64Array::from(values)),
                Arc::new(StringArray::from(fields_json_refs)),
            ],
        )
        .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, schema, Some(self.writer_props()))
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        debug!(
            "Wrote {} custom data points to {}",
            points.len(),
            path.display()
        );
        Ok(())
    }

    fn ensure_dir(&self, path: &Path) -> LeanResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn lock_partition(&self, path: &Path) -> LeanResult<PartitionLock> {
        self.ensure_dir(path)?;
        let lock_path = path.with_file_name(".data.parquet.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&lock_path)?;
        file.lock_exclusive()
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
        Ok(PartitionLock { file })
    }

    fn write_trade_bars_atomic(&self, bars: &[TradeBar], path: &Path) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_trade_bars(bars, &tmp)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn write_quote_bars_atomic(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_quote_bars(bars, &tmp)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn write_ticks_atomic(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_ticks(ticks, &tmp)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }
}

struct PartitionLock {
    file: fs::File,
}

impl Drop for PartitionLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

fn temp_path(path: &Path) -> std::path::PathBuf {
    path.with_file_name(format!("data.parquet.tmp.{}", uuid::Uuid::new_v4()))
}

fn partition_has_all_trade_rows(path: &Path, bars: &[TradeBar]) -> LeanResult<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = crate::reader::ParquetReader::new().read_trade_bar_partition(
        path,
        &bars[0].symbol,
        &Default::default(),
    )?;
    Ok(bars.iter().all(|bar| {
        existing.iter().any(|existing| {
            existing.symbol.id.sid == bar.symbol.id.sid
                && existing.time.0 == bar.time.0
                && existing.open == bar.open
                && existing.high == bar.high
                && existing.low == bar.low
                && existing.close == bar.close
                && existing.volume == bar.volume
                && existing.period == bar.period
        })
    }))
}

fn partition_has_all_quote_rows(path: &Path, bars: &[QuoteBar]) -> LeanResult<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = crate::reader::ParquetReader::new().read_quote_bar_partition(
        path,
        &bars[0].symbol,
        &Default::default(),
    )?;
    Ok(bars.iter().all(|bar| {
        existing.iter().any(|existing| {
            existing.symbol.id.sid == bar.symbol.id.sid
                && existing.time.0 == bar.time.0
                && existing.bid == bar.bid
                && existing.ask == bar.ask
                && existing.last_bid_size == bar.last_bid_size
                && existing.last_ask_size == bar.last_ask_size
                && existing.period == bar.period
        })
    }))
}

fn partition_has_all_tick_rows(path: &Path, ticks: &[Tick]) -> LeanResult<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let existing = crate::reader::ParquetReader::new().read_tick_partition(
        path,
        &ticks[0].symbol,
        &Default::default(),
    )?;
    Ok(ticks.iter().all(|tick| {
        existing.iter().any(|existing| {
            existing.symbol.id.sid == tick.symbol.id.sid
                && existing.time.0 == tick.time.0
                && existing.tick_type == tick.tick_type
                && existing.value == tick.value
                && existing.quantity == tick.quantity
                && existing.bid_price == tick.bid_price
                && existing.ask_price == tick.ask_price
                && existing.bid_size == tick.bid_size
                && existing.ask_size == tick.ask_size
        })
    }))
}

fn dedupe_trade_bars(bars: &mut Vec<TradeBar>) {
    let mut seen = HashSet::new();
    bars.retain(|bar| seen.insert((bar.symbol.id.sid, bar.time.0)));
}

fn dedupe_quote_bars(bars: &mut Vec<QuoteBar>) {
    let mut seen = HashSet::new();
    bars.retain(|bar| seen.insert((bar.symbol.id.sid, bar.time.0)));
}

fn dedupe_ticks(ticks: &mut Vec<Tick>) {
    let mut seen = HashSet::new();
    ticks.retain(|tick| seen.insert((tick.symbol.id.sid, tick.time.0, tick.tick_type)));
}

fn sort_trade_bars_for_predicate_pruning(bars: &mut [TradeBar]) {
    bars.sort_by_key(|bar| (bar.symbol.id.sid, bar.time.0));
}

fn sort_quote_bars_for_predicate_pruning(bars: &mut [QuoteBar]) {
    bars.sort_by_key(|bar| (bar.symbol.id.sid, bar.time.0));
}

fn sort_ticks_for_predicate_pruning(ticks: &mut [Tick]) {
    ticks.sort_by_key(|tick| (tick.symbol.id.sid, tick.time.0, tick.tick_type as u8));
}
