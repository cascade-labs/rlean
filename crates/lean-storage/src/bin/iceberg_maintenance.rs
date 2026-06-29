use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lean_core::{DateTime, Market, Resolution, Symbol, TickType};
use lean_storage::{IcebergStore, QueryParams};

#[derive(Parser)]
#[command(about = "Inspect and maintain local rlean Iceberg cache tables")]
struct Args {
    /// Data root that contains the local `iceberg/` warehouse.
    #[arg(long, default_value = "data", env = "RLEAN_DATA")]
    data: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print metadata file counts for the Iceberg warehouse.
    Report,
    /// Report duplicate market trade bar keys in a scoped window.
    Duplicates {
        #[arg(long)]
        ticker: String,
        #[arg(long, default_value = "daily")]
        resolution: String,
        #[arg(long)]
        start: chrono::NaiveDate,
        #[arg(long)]
        end: chrono::NaiveDate,
    },
    /// Print trade bars visible through the Iceberg catalog in a scoped window.
    Rows {
        #[arg(long)]
        ticker: String,
        #[arg(long, default_value = "daily")]
        resolution: String,
        #[arg(long)]
        start: chrono::NaiveDate,
        #[arg(long)]
        end: chrono::NaiveDate,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Report => report(&args.data),
        Command::Duplicates {
            ticker,
            resolution,
            start,
            end,
        } => {
            report_duplicates(
                &args.data,
                &ticker,
                parse_resolution(&resolution)?,
                start,
                end,
            )
            .await
        }
        Command::Rows {
            ticker,
            resolution,
            start,
            end,
            limit,
        } => {
            print_trade_rows(
                &args.data,
                &ticker,
                parse_resolution(&resolution)?,
                start,
                end,
                limit,
            )
            .await
        }
    }
}

fn report(data_root: &Path) -> Result<()> {
    let iceberg = data_root.join("iceberg");
    let metadata_root = iceberg.join("lean");
    println!("warehouse={}", iceberg.display());
    for table in [
        "market_trade_bars",
        "market_quote_bars",
        "market_ticks",
        "custom_points",
        "option_eod_bars",
        "option_universe",
        "factor_files",
        "map_files",
    ] {
        let metadata = metadata_root.join(table).join("metadata");
        let metadata_files = count_extension(&metadata, "json")?;
        let manifest_files = count_extension(&metadata, "avro")?;
        println!("{table}: metadata_json={metadata_files} manifest_avro={manifest_files}");
    }
    Ok(())
}

async fn report_duplicates(
    data_root: &Path,
    ticker: &str,
    resolution: Resolution,
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
) -> Result<()> {
    let store = IcebergStore::connect_local(data_root).await?;
    let symbol = Symbol::create_equity(ticker, &Market::usa());
    let sid = symbol.id.sid;
    let start_dt = DateTime::from(start.and_hms_opt(0, 0, 0).context("invalid start date")?);
    let end_dt = DateTime::from(end.and_hms_opt(23, 59, 59).context("invalid end date")?);
    let params = QueryParams::new()
        .with_day_range(start_dt, end_dt)
        .with_bar_range(start_dt, end_dt)
        .with_symbols(vec![sid]);
    let grouped = store
        .scan_trade_bar_partitions_grouped(
            &HashMap::from([(sid, symbol)]),
            resolution,
            TickType::Trade,
            &params,
        )
        .await?;
    let rows = grouped.get(&sid).cloned().unwrap_or_default();
    let mut seen = HashSet::new();
    let mut duplicates = 0usize;
    for row in &rows {
        if !seen.insert((row.symbol.id.sid, row.end_time.0)) {
            duplicates += 1;
        }
    }
    println!(
        "ticker={} resolution={} rows={} duplicate_keys={}",
        ticker,
        resolution.folder_name(),
        rows.len(),
        duplicates
    );
    Ok(())
}

async fn print_trade_rows(
    data_root: &Path,
    ticker: &str,
    resolution: Resolution,
    start: chrono::NaiveDate,
    end: chrono::NaiveDate,
    limit: usize,
) -> Result<()> {
    let store = IcebergStore::connect_local(data_root).await?;
    let symbol = Symbol::create_equity(ticker, &Market::usa());
    let sid = symbol.id.sid;
    let start_dt = DateTime::from(start.and_hms_opt(0, 0, 0).context("invalid start date")?);
    let end_dt = DateTime::from(end.and_hms_opt(23, 59, 59).context("invalid end date")?);
    let params = QueryParams::new()
        .with_day_range(start_dt, end_dt)
        .with_bar_range(start_dt, end_dt)
        .with_symbols(vec![sid]);
    let grouped = store
        .scan_trade_bar_partitions_grouped(
            &HashMap::from([(sid, symbol)]),
            resolution,
            TickType::Trade,
            &params,
        )
        .await?;
    let rows = grouped.get(&sid).cloned().unwrap_or_default();
    println!(
        "ticker={} resolution={} rows={}",
        ticker,
        resolution.folder_name(),
        rows.len()
    );
    for row in rows.iter().take(limit) {
        println!(
            "{} {} open={} high={} low={} close={} volume={}",
            row.time, row.end_time, row.open, row.high, row.low, row.close, row.volume
        );
    }
    Ok(())
}

fn count_extension(path: &Path, extension: &str) -> Result<usize> {
    if !path.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in
        std::fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))?
    {
        let entry = entry?;
        if entry
            .path()
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case(extension))
            .unwrap_or(false)
        {
            count += 1;
        }
    }
    Ok(count)
}

fn parse_resolution(value: &str) -> Result<Resolution> {
    match value.to_ascii_lowercase().as_str() {
        "tick" => Ok(Resolution::Tick),
        "second" => Ok(Resolution::Second),
        "minute" => Ok(Resolution::Minute),
        "hour" | "hourly" => Ok(Resolution::Hour),
        "daily" | "day" => Ok(Resolution::Daily),
        other => anyhow::bail!("unsupported resolution {other}"),
    }
}
