use crate::predicate::QueryParams;
use crate::reader::ParquetReader;
use crate::schema::{FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow};
use crate::{convert, schema};
use arrow_array::{Float64Array, Int64Array, RecordBatch, StringArray};
use fs2::FileExt;
use lean_core::{LeanError, Result as LeanResult};
use lean_data::{QuoteBar, Tick, TradeBar};
use parquet::{
    arrow::ArrowWriter,
    basic::{Compression, ZstdLevel},
    file::properties::{EnabledStatistics, WriterProperties},
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterCompression {
    Snappy,
    Zstd,
    Uncompressed,
}

#[derive(Debug, Clone)]
pub struct WriterConfig {
    pub compression: WriterCompression,
    pub compression_level: i32,
    pub row_group_size: usize,
    pub write_statistics: bool,
    pub bloom_filter: bool,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            compression: WriterCompression::Snappy,
            compression_level: 1,
            row_group_size: 8_192,
            write_statistics: true,
            bloom_filter: false,
        }
    }
}

pub struct ParquetWriter {
    config: WriterConfig,
}

impl ParquetWriter {
    pub fn new(config: WriterConfig) -> Self {
        Self { config }
    }

    pub fn write_trade_bars(&self, bars: &[TradeBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }
        self.write_batch(convert::trade_bars_to_record_batch(bars), path)
    }

    pub fn write_quote_bars(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }
        self.write_batch(convert::quote_bars_to_record_batch(bars), path)
    }

    pub fn write_ticks(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        if ticks.is_empty() {
            return Ok(());
        }
        self.write_batch(convert::ticks_to_record_batch(ticks), path)
    }

    pub fn write_option_eod_bars(&self, rows: &[OptionEodBar], path: &Path) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.write_batch(convert::option_eod_bars_to_record_batch(rows), path)
    }

    pub fn write_option_universe(&self, rows: &[OptionUniverseRow], path: &Path) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        self.write_batch(convert::option_universe_rows_to_record_batch(rows), path)
    }

    pub fn write_factor_file(&self, rows: &[FactorFileEntry], path: &Path) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            schema::factor_file_schema(),
            vec![
                Arc::new(Int64Array::from(
                    rows.iter().map(|row| row.date_ns()).collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    rows.iter().map(|row| row.price_factor).collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    rows.iter().map(|row| row.split_factor).collect::<Vec<_>>(),
                )),
                Arc::new(Float64Array::from(
                    rows.iter()
                        .map(|row| row.reference_price)
                        .collect::<Vec<_>>(),
                )),
            ],
        )
        .map_err(|e| LeanError::DataError(e.to_string()))?;
        self.write_batch(batch, path)
    }

    pub fn write_map_file(&self, rows: &[MapFileEntry], path: &Path) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            schema::map_file_schema(),
            vec![
                Arc::new(Int64Array::from(
                    rows.iter().map(|row| row.date_ns()).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from(
                    rows.iter()
                        .map(|row| row.ticker.as_str())
                        .collect::<Vec<_>>(),
                )),
            ],
        )
        .map_err(|e| LeanError::DataError(e.to_string()))?;
        self.write_batch(batch, path)
    }

    pub fn merge_trade_bar_partition(&self, bars: &[TradeBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let _lock = self.lock_partition(path)?;
        let replacement_sids = bars
            .iter()
            .map(|bar| bar.symbol.id.sid)
            .collect::<HashSet<_>>();
        let mut merged = if path.exists() {
            ParquetReader::new()
                .read_trade_bar_partition(path, &bars[0].symbol, &QueryParams::default())?
                .into_iter()
                .filter(|bar| !replacement_sids.contains(&bar.symbol.id.sid))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        merged.extend_from_slice(bars);
        merged.sort_by_key(|bar| (bar.symbol.id.sid, bar.time.0));
        merged.dedup_by_key(|bar| (bar.symbol.id.sid, bar.time.0));
        self.write_trade_bars_atomic(&merged, path)
    }

    pub fn merge_quote_bar_partition(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let _lock = self.lock_partition(path)?;
        let replacement_sids = bars
            .iter()
            .map(|bar| bar.symbol.id.sid)
            .collect::<HashSet<_>>();
        let mut merged = if path.exists() {
            ParquetReader::new()
                .read_quote_bar_partition(path, &bars[0].symbol, &QueryParams::default())?
                .into_iter()
                .filter(|bar| !replacement_sids.contains(&bar.symbol.id.sid))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        merged.extend_from_slice(bars);
        merged.sort_by_key(|bar| (bar.symbol.id.sid, bar.time.0));
        merged.dedup_by_key(|bar| (bar.symbol.id.sid, bar.time.0));
        self.write_quote_bars_atomic(&merged, path)
    }

    pub fn merge_tick_partition(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        if ticks.is_empty() {
            return Ok(());
        }
        let _lock = self.lock_partition(path)?;
        let replacement_sids = ticks
            .iter()
            .map(|tick| tick.symbol.id.sid)
            .collect::<HashSet<_>>();
        let mut merged = if path.exists() {
            ParquetReader::new()
                .read_tick_partition(path, &ticks[0].symbol, &QueryParams::default())?
                .into_iter()
                .filter(|tick| !replacement_sids.contains(&tick.symbol.id.sid))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        merged.extend_from_slice(ticks);
        merged.sort_by_key(|tick| (tick.symbol.id.sid, tick.time.0, tick.tick_type as u8));
        merged.dedup_by_key(|tick| {
            (
                tick.symbol.id.sid,
                tick.time.0,
                tick.tick_type as u8,
                tick.value,
                tick.quantity,
                tick.bid_price,
                tick.ask_price,
            )
        });
        self.write_ticks_atomic(&merged, path)
    }

    pub fn merge_option_universe_partition(
        &self,
        rows: &[OptionUniverseRow],
        path: &Path,
    ) -> LeanResult<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let _lock = self.lock_partition(path)?;
        let replacement_underlyings = rows
            .iter()
            .map(|row| row.underlying.to_ascii_uppercase())
            .collect::<HashSet<_>>();
        let mut merged = if path.exists() {
            ParquetReader::new()
                .read_option_universe(&[path.to_path_buf()])?
                .into_iter()
                .filter(|row| {
                    !replacement_underlyings.contains(&row.underlying.to_ascii_uppercase())
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        merged.extend_from_slice(rows);
        merged.sort_by(|a, b| {
            (
                a.underlying.as_str(),
                a.expiration,
                a.strike,
                a.right.as_str(),
                a.symbol_value.as_str(),
            )
                .cmp(&(
                    b.underlying.as_str(),
                    b.expiration,
                    b.strike,
                    b.right.as_str(),
                    b.symbol_value.as_str(),
                ))
        });
        merged.dedup_by(|a, b| {
            a.underlying.eq_ignore_ascii_case(&b.underlying)
                && a.symbol_value == b.symbol_value
                && a.expiration == b.expiration
                && a.strike == b.strike
                && a.right == b.right
        });
        self.write_option_universe_atomic(&merged, path)
    }

    fn write_batch(&self, batch: arrow_array::RecordBatch, path: &Path) -> LeanResult<()> {
        self.ensure_dir(path)?;
        let file = fs::File::create(path)?;
        let mut writer = ArrowWriter::try_new(file, batch.schema(), Some(self.writer_props()))
            .map_err(|e| LeanError::DataError(e.to_string()))?;
        writer
            .write(&batch)
            .map_err(|e| LeanError::DataError(e.to_string()))?;
        writer
            .close()
            .map_err(|e| LeanError::DataError(e.to_string()))?;
        Ok(())
    }

    fn write_trade_bars_atomic(&self, bars: &[TradeBar], path: &Path) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_trade_bars(bars, &tmp)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn write_quote_bars_atomic(&self, bars: &[QuoteBar], path: &Path) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_quote_bars(bars, &tmp)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn write_ticks_atomic(&self, ticks: &[Tick], path: &Path) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_ticks(ticks, &tmp)?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn write_option_universe_atomic(
        &self,
        rows: &[OptionUniverseRow],
        path: &Path,
    ) -> LeanResult<()> {
        let tmp = temp_path(path);
        self.write_option_universe(rows, &tmp)?;
        fs::rename(tmp, path)?;
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
            .open(lock_path)?;
        file.lock_exclusive()
            .map_err(|e| LeanError::DataError(e.to_string()))?;
        Ok(PartitionLock { file })
    }

    fn ensure_dir(&self, path: &Path) -> LeanResult<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn writer_props(&self) -> WriterProperties {
        let compression = match self.config.compression {
            WriterCompression::Snappy => Compression::SNAPPY,
            WriterCompression::Zstd => Compression::ZSTD(
                ZstdLevel::try_new(self.config.compression_level)
                    .unwrap_or_else(|_| ZstdLevel::try_new(1).unwrap()),
            ),
            WriterCompression::Uncompressed => Compression::UNCOMPRESSED,
        };
        let mut builder = WriterProperties::builder()
            .set_compression(compression)
            .set_max_row_group_size(self.config.row_group_size)
            .set_statistics_enabled(if self.config.write_statistics {
                EnabledStatistics::Chunk
            } else {
                EnabledStatistics::None
            });
        if self.config.bloom_filter {
            builder = builder.set_bloom_filter_enabled(true);
        }
        builder.build()
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

fn temp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!("data.parquet.tmp.{}", uuid::Uuid::new_v4()))
}
