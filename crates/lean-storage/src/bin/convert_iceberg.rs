use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use arrow::compute::concat_batches;
use arrow_array::{Array, ArrayRef, Int64Array, RecordBatch, StringArray, UInt64Array};
use arrow_cast::cast;
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use clap::Parser;
use lean_core::{Resolution, SecurityType, TickType};
use lean_storage::IcebergStore;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

const MARKET_TRADE_IMPORT_FILES_PER_APPEND: usize = 64;

#[derive(Parser)]
#[command(about = "One-time migration from legacy rlean Parquet data into local Iceberg tables")]
struct Args {
    /// Legacy Parquet data root to read from.
    #[arg(long, default_value = "data", env = "RLEAN_DATA")]
    data: PathBuf,

    /// Iceberg warehouse data root. Defaults to the source data root.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Stop after converting this many source files. Useful for smoke tests.
    #[arg(long)]
    limit_files: Option<usize>,

    /// Stop after appending this many rows. Useful for smoke tests.
    #[arg(long)]
    limit_rows: Option<usize>,

    /// Print each converted source file.
    #[arg(long, short = 'v')]
    verbose: bool,

    /// Only convert one Iceberg table name, e.g. factor_files.
    #[arg(long)]
    table: Option<String>,

    /// Drop and recreate the selected Iceberg table before converting.
    #[arg(long)]
    reset_table: bool,

    /// Read raw data files from an existing Iceberg table directory instead of legacy paths.
    #[arg(long)]
    iceberg_source_table: Option<PathBuf>,
}

#[derive(Default)]
struct Stats {
    files_seen: usize,
    files_converted: usize,
    files_failed: usize,
    files_skipped: usize,
    rows_converted: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if !args.data.exists() {
        bail!("source data root '{}' does not exist", args.data.display());
    }
    let output = args.output.clone().unwrap_or_else(|| args.data.clone());
    let store = IcebergStore::connect_local(&output).await?;
    if args.reset_table {
        let table = args
            .table
            .as_deref()
            .context("--reset-table requires --table")?;
        store.reset_table(table).await?;
    }

    if args.table.as_deref() == Some("factor_files") {
        let stats = convert_factor_files_batched(&store, &args).await?;
        println!(
            "Iceberg conversion complete: {} files converted, {} files failed, {} files skipped, {} rows appended into {}",
            stats.files_converted,
            stats.files_failed,
            stats.files_skipped,
            stats.rows_converted,
            store.warehouse_root().display()
        );
        return Ok(());
    }

    if args.table.as_deref() == Some("market_trade_bars") {
        if let Some(source_table) = args.iceberg_source_table.as_ref() {
            let stats = convert_iceberg_market_trade_files(&store, source_table, &args).await?;
            println!(
                "Iceberg conversion complete: {} files converted, {} files failed, {} files skipped, {} rows appended into {}",
                stats.files_converted,
                stats.files_failed,
                stats.files_skipped,
                stats.rows_converted,
                store.warehouse_root().display()
            );
            return Ok(());
        }
    }

    let mut stats = Stats::default();

    for path in parquet_files_under(&args.data)? {
        if reached_limit(stats.files_converted, args.limit_files)
            || reached_limit(stats.rows_converted, args.limit_rows)
        {
            break;
        }
        stats.files_seen += 1;
        let Some(kind) = classify_legacy_parquet(&args.data, &path) else {
            stats.files_skipped += 1;
            continue;
        };
        if let Some(table) = args.table.as_deref() {
            if kind.table_name() != table {
                stats.files_skipped += 1;
                continue;
            }
        }
        let mut converted_file = false;
        let mut failed_file = false;
        for batch in read_parquet_batches(&path)? {
            if batch.num_rows() == 0 {
                continue;
            }
            let remaining = args
                .limit_rows
                .map(|limit| limit.saturating_sub(stats.rows_converted));
            let rows = remaining.map_or(batch.num_rows(), |remaining| {
                remaining.min(batch.num_rows())
            });
            if rows == 0 {
                break;
            }
            if let Err(err) = append_legacy_batch(&store, &kind, batch.slice(0, rows)).await {
                stats.files_failed += 1;
                failed_file = true;
                eprintln!("failed {}: {err:#}", path.display());
                break;
            }
            stats.rows_converted += rows;
            converted_file = true;
            if rows < batch.num_rows() {
                break;
            }
        }
        if converted_file {
            stats.files_converted += 1;
            if args.verbose {
                println!("converted {}", path.display());
            }
        } else if !failed_file {
            stats.files_skipped += 1;
        }
    }

    println!(
        "Iceberg conversion complete: {} files converted, {} files failed, {} files skipped, {} rows appended into {}",
        stats.files_converted,
        stats.files_failed,
        stats.files_skipped,
        stats.rows_converted,
        store.warehouse_root().display()
    );
    Ok(())
}

fn parquet_files_under(root: &Path) -> Result<Vec<PathBuf>> {
    let pattern = root.join("**").join("*.parquet");
    let pattern = pattern.to_string_lossy().to_string();
    let mut files = glob::glob(&pattern)
        .with_context(|| format!("invalid parquet glob pattern {pattern}"))?
        .filter_map(|entry| entry.ok())
        .filter(|path| {
            !path
                .components()
                .any(|component| component.as_os_str() == "iceberg")
        })
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn parquet_files_under_including_iceberg(root: &Path) -> Result<Vec<PathBuf>> {
    let pattern = root.join("**").join("*.parquet");
    let pattern = pattern.to_string_lossy().to_string();
    let mut files = glob::glob(&pattern)
        .with_context(|| format!("invalid parquet glob pattern {pattern}"))?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    files.sort();
    Ok(files)
}

fn read_parquet_batches(path: &Path) -> Result<Vec<arrow_array::RecordBatch>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .with_context(|| format!("failed to read parquet metadata from {}", path.display()))?
        .build()
        .with_context(|| format!("failed to create parquet reader for {}", path.display()))?;
    reader
        .map(|batch| batch.with_context(|| format!("failed to read {}", path.display())))
        .collect()
}

async fn convert_factor_files_batched(store: &IcebergStore, args: &Args) -> Result<Stats> {
    let mut stats = Stats::default();
    let mut batches = Vec::new();

    for path in parquet_files_under(&args.data)? {
        if reached_limit(stats.files_converted, args.limit_files)
            || reached_limit(stats.rows_converted, args.limit_rows)
        {
            break;
        }
        stats.files_seen += 1;
        let Some(LegacyParquetKind::FactorFile { market, ticker }) =
            classify_legacy_parquet(&args.data, &path)
        else {
            stats.files_skipped += 1;
            continue;
        };

        let mut converted_file = false;
        for batch in read_parquet_batches(&path)? {
            if batch.num_rows() == 0 {
                continue;
            }
            let remaining = args
                .limit_rows
                .map(|limit| limit.saturating_sub(stats.rows_converted));
            let rows = remaining.map_or(batch.num_rows(), |remaining| {
                remaining.min(batch.num_rows())
            });
            if rows == 0 {
                break;
            }
            batches.push(with_factor_partitions(
                batch.slice(0, rows),
                &market,
                &ticker,
            )?);
            stats.rows_converted += rows;
            converted_file = true;
            if rows < batch.num_rows() {
                break;
            }
        }

        if converted_file {
            stats.files_converted += 1;
            if args.verbose {
                println!("converted {}", path.display());
            }
        } else {
            stats.files_skipped += 1;
        }
    }

    if !batches.is_empty() {
        let schema = batches[0].schema();
        let batch = concat_batches(&schema, batches.iter())
            .context("failed to concatenate factor file batches")?;
        store
            .append_factor_record_batch_with_partitions(batch)
            .await?;
    }

    Ok(stats)
}

fn with_factor_partitions(batch: RecordBatch, market: &str, ticker: &str) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let mut fields = batch
        .schema()
        .fields()
        .iter()
        .map(|field| field.as_ref().clone())
        .collect::<Vec<_>>();
    fields.push(Field::new("market", DataType::Utf8, false));
    fields.push(Field::new("ticker", DataType::Utf8, false));

    let mut columns = batch.columns().to_vec();
    columns.push(Arc::new(StringArray::from(vec![market.to_lowercase(); rows])) as ArrayRef);
    columns.push(Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])) as ArrayRef);

    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(fields)),
        columns,
    )?)
}

async fn convert_iceberg_market_trade_files(
    store: &IcebergStore,
    source_table: &Path,
    args: &Args,
) -> Result<Stats> {
    let mut stats = Stats::default();
    let mut batches = Vec::new();
    let mut pending_files = 0usize;
    let mut kind: Option<(SecurityType, String, Resolution)> = None;
    let files = parquet_files_under_including_iceberg(source_table)?;

    for path in files {
        if reached_limit(stats.files_converted, args.limit_files)
            || reached_limit(stats.rows_converted, args.limit_rows)
        {
            break;
        }
        stats.files_seen += 1;
        let Some((security_type, market, resolution)) = iceberg_market_trade_file_kind(&path)
        else {
            stats.files_skipped += 1;
            continue;
        };
        let file_kind = (security_type, market, resolution);
        match &kind {
            Some(existing) if existing != &file_kind => {
                anyhow::bail!(
                    "mixed market trade kinds are not supported in one batched import: {:?} and {:?}",
                    existing,
                    file_kind
                );
            }
            None => kind = Some(file_kind.clone()),
            _ => {}
        }

        let mut converted_file = false;
        for batch in read_parquet_batches(&path)? {
            if batch.num_rows() == 0 {
                continue;
            }
            let remaining = args
                .limit_rows
                .map(|limit| limit.saturating_sub(stats.rows_converted));
            let rows = remaining.map_or(batch.num_rows(), |remaining| {
                remaining.min(batch.num_rows())
            });
            if rows == 0 {
                break;
            }
            batches.push(trade_bar_base_batch(batch.slice(0, rows))?);
            stats.rows_converted += rows;
            converted_file = true;
            if rows < batch.num_rows() {
                break;
            }
        }
        if converted_file {
            stats.files_converted += 1;
            pending_files += 1;
            if args.verbose {
                println!("converted {}", path.display());
            }
            if pending_files >= MARKET_TRADE_IMPORT_FILES_PER_APPEND {
                append_market_trade_import_batches(
                    store,
                    &mut batches,
                    kind.as_ref()
                        .context("market trade import had rows but no partition kind")?,
                )
                .await?;
                pending_files = 0;
            }
        } else {
            stats.files_skipped += 1;
        }
    }

    if let Some(kind) = kind.as_ref() {
        append_market_trade_import_batches(store, &mut batches, kind).await?;
    }

    Ok(stats)
}

async fn append_market_trade_import_batches(
    store: &IcebergStore,
    batches: &mut Vec<RecordBatch>,
    kind: &(SecurityType, String, Resolution),
) -> Result<()> {
    if batches.is_empty() {
        return Ok(());
    }
    let schema = batches[0].schema();
    let pending = std::mem::take(batches);
    let batch = concat_batches(&schema, pending.iter())
        .context("failed to concatenate market trade batches")?;
    let (security_type, market, resolution) = kind;
    store
        .append_trade_record_batch(batch, *security_type, market, *resolution, TickType::Trade)
        .await
}

fn iceberg_market_trade_file_kind(path: &Path) -> Option<(SecurityType, String, Resolution)> {
    let mut security_type = None;
    let mut market = None;
    let mut resolution = None;
    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        let Some((key, value)) = text.split_once('=') else {
            continue;
        };
        match key {
            "security_type" => security_type = parse_security_type(value),
            "market" => market = Some(value.to_string()),
            "resolution" => resolution = parse_resolution(value),
            _ => {}
        }
    }
    Some((security_type?, market?, resolution?))
}

fn trade_bar_base_batch(batch: RecordBatch) -> Result<RecordBatch> {
    let wanted = [
        ("time_ns", DataType::Int64),
        ("end_time_ns", DataType::Int64),
        ("symbol_sid", DataType::Int64),
        ("symbol_value", DataType::Utf8),
        ("open", DataType::Int64),
        ("high", DataType::Int64),
        ("low", DataType::Int64),
        ("close", DataType::Int64),
        ("volume", DataType::Int64),
        ("period_ns", DataType::Int64),
    ];
    let mut fields = Vec::with_capacity(wanted.len());
    let mut columns = Vec::with_capacity(wanted.len());
    for (name, ty) in wanted {
        let index = batch
            .schema()
            .index_of(name)
            .with_context(|| format!("required trade-bar column {name} missing"))?;
        fields.push(Field::new(name, ty.clone(), false));
        columns.push(cast_trade_bar_column(
            name,
            batch.column(index).clone(),
            &ty,
        )?);
    }
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(fields)),
        columns,
    )?)
}

fn cast_trade_bar_column(name: &str, column: ArrayRef, data_type: &DataType) -> Result<ArrayRef> {
    if column.data_type() == data_type {
        return Ok(column);
    }
    if name == "symbol_sid"
        && column.data_type() == &DataType::UInt64
        && data_type == &DataType::Int64
    {
        let values = column
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| anyhow!("symbol_sid must be uint64"))?;
        if values.null_count() == 0 {
            let converted = (0..values.len())
                .map(|row| values.value(row) as i64)
                .collect::<Vec<_>>();
            return Ok(Arc::new(Int64Array::from(converted)));
        }
        let converted = (0..values.len())
            .map(|row| values.is_valid(row).then(|| values.value(row) as i64))
            .collect::<Vec<_>>();
        return Ok(Arc::new(Int64Array::from(converted)));
    }
    Ok(cast(&column, data_type)?)
}

#[derive(Debug, Clone)]
enum LegacyParquetKind {
    Market {
        table: MarketTable,
        security_type: SecurityType,
        market: String,
        resolution: Resolution,
        tick_type: TickType,
    },
    OptionUniverse,
    OptionEod,
    Custom {
        source_type: String,
        ticker: String,
    },
    FactorFile {
        market: String,
        ticker: String,
    },
    MapFile {
        market: String,
        ticker: String,
    },
}

impl LegacyParquetKind {
    fn table_name(&self) -> &'static str {
        match self {
            Self::Market {
                table: MarketTable::Trade,
                ..
            } => "market_trade_bars",
            Self::Market {
                table: MarketTable::Quote,
                ..
            } => "market_quote_bars",
            Self::Market {
                table: MarketTable::Tick,
                ..
            } => "market_ticks",
            Self::OptionUniverse => "option_universe",
            Self::OptionEod => "option_eod_bars",
            Self::Custom { .. } => "custom_points",
            Self::FactorFile { .. } => "factor_files",
            Self::MapFile { .. } => "map_files",
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MarketTable {
    Trade,
    Quote,
    Tick,
}

async fn append_legacy_batch(
    store: &IcebergStore,
    kind: &LegacyParquetKind,
    batch: arrow_array::RecordBatch,
) -> Result<()> {
    match kind {
        LegacyParquetKind::Market {
            table,
            security_type,
            market,
            resolution,
            tick_type,
        } => match table {
            MarketTable::Trade => {
                store
                    .append_trade_record_batch(
                        batch,
                        *security_type,
                        market,
                        *resolution,
                        *tick_type,
                    )
                    .await
            }
            MarketTable::Quote => {
                store
                    .append_quote_record_batch(
                        batch,
                        *security_type,
                        market,
                        *resolution,
                        *tick_type,
                    )
                    .await
            }
            MarketTable::Tick => {
                store
                    .append_tick_record_batch(
                        batch,
                        *security_type,
                        market,
                        *resolution,
                        *tick_type,
                    )
                    .await
            }
        },
        LegacyParquetKind::OptionUniverse => store.append_option_universe_record_batch(batch).await,
        LegacyParquetKind::OptionEod => store.append_option_eod_record_batch(batch).await,
        LegacyParquetKind::Custom {
            source_type,
            ticker,
        } => {
            store
                .append_custom_record_batch(source_type, ticker, batch)
                .await
        }
        LegacyParquetKind::FactorFile { market, ticker } => {
            store
                .append_factor_record_batch(market, ticker, batch)
                .await
        }
        LegacyParquetKind::MapFile { market, ticker } => {
            store.append_map_record_batch(market, ticker, batch).await
        }
    }
}

fn classify_legacy_parquet(root: &Path, path: &Path) -> Option<LegacyParquetKind> {
    let relative = path.strip_prefix(root).ok()?;
    let parts = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>();
    if parts.len() >= 3 && parts[0] == "custom" {
        let ticker = path.file_stem()?.to_string_lossy();
        let ticker = if ticker == "history" {
            parts.get(2)?.clone()
        } else {
            ticker.to_string()
        };
        return Some(LegacyParquetKind::Custom {
            source_type: parts[1].clone(),
            ticker,
        });
    }
    if parts.len() == 4 && parts[0] == "equity" && parts[2] == "factor_files" {
        return Some(LegacyParquetKind::FactorFile {
            market: parts[1].clone(),
            ticker: path.file_stem()?.to_string_lossy().to_string(),
        });
    }
    if parts.len() == 4 && parts[0] == "equity" && parts[2] == "map_files" {
        return Some(LegacyParquetKind::MapFile {
            market: parts[1].clone(),
            ticker: path.file_stem()?.to_string_lossy().to_string(),
        });
    }
    if parts.len() >= 6 && parts[0] == "option" && parts[3] == "universe" {
        return Some(LegacyParquetKind::OptionUniverse);
    }
    if parts.len() >= 3
        && parts[0] == "option"
        && path.file_stem()?.to_string_lossy().ends_with("_eod")
    {
        return Some(LegacyParquetKind::OptionEod);
    }
    if parts.len() < 6 || path.file_name()?.to_string_lossy() != "data.parquet" {
        return None;
    }
    let security_type = parse_security_type(&parts[0])?;
    let resolution = parse_resolution(&parts[2])?;
    let tick_type = parse_tick_type(&parts[3])?;
    let table = match tick_type {
        TickType::Trade => MarketTable::Trade,
        TickType::Quote => MarketTable::Quote,
        TickType::OpenInterest => MarketTable::Tick,
    };
    Some(LegacyParquetKind::Market {
        table,
        security_type,
        market: parts[1].clone(),
        resolution,
        tick_type,
    })
}

fn parse_security_type(value: &str) -> Option<SecurityType> {
    match value.to_ascii_lowercase().as_str() {
        "base" => Some(SecurityType::Base),
        "equity" => Some(SecurityType::Equity),
        "option" => Some(SecurityType::Option),
        "commodity" => Some(SecurityType::Commodity),
        "forex" => Some(SecurityType::Forex),
        "future" => Some(SecurityType::Future),
        "cfd" => Some(SecurityType::Cfd),
        "crypto" => Some(SecurityType::Crypto),
        "futureoption" | "future_option" | "future-option" => Some(SecurityType::FutureOption),
        "indexoption" | "index_option" | "index-option" => Some(SecurityType::IndexOption),
        "index" => Some(SecurityType::Index),
        "cryptofuture" | "crypto_future" | "crypto-future" => Some(SecurityType::CryptoFuture),
        _ => None,
    }
}

fn parse_resolution(value: &str) -> Option<Resolution> {
    match value.to_ascii_lowercase().as_str() {
        "tick" => Some(Resolution::Tick),
        "second" => Some(Resolution::Second),
        "minute" => Some(Resolution::Minute),
        "hour" => Some(Resolution::Hour),
        "daily" | "day" => Some(Resolution::Daily),
        _ => None,
    }
}

fn parse_tick_type(value: &str) -> Option<TickType> {
    match value.to_ascii_lowercase().as_str() {
        "trade" | "trades" => Some(TickType::Trade),
        "quote" | "quotes" => Some(TickType::Quote),
        "openinterest" | "open_interest" | "open-interest" => Some(TickType::OpenInterest),
        _ => None,
    }
}

fn reached_limit(count: usize, limit: Option<usize>) -> bool {
    limit.is_some_and(|limit| count >= limit)
}
