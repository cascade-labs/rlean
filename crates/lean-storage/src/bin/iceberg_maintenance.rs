use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lean_storage::IcebergStore;

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
    /// Print data/metadata file counts for each cache table.
    Report,
    /// Rewrite tables into a small number of files under a single fresh
    /// snapshot, collapsing the accumulated snapshot/manifest chain.
    ///
    /// iceberg-rust has no snapshot expiration, so append-only ingest grows the
    /// metadata without bound; this reclaims it and restores fast query
    /// planning. Row content is preserved exactly.
    Compact {
        /// Tables to compact. Defaults to the market + custom cache tables.
        #[arg(long = "table")]
        tables: Vec<String>,
    },
    /// Recover a table from staged chunks left behind by an interrupted
    /// compaction (re-inserts `.compact_tmp_<table>` into the empty table).
    Restore {
        /// Tables to restore from their staging directories.
        #[arg(long = "table", required = true)]
        tables: Vec<String>,
    },
    /// Print the live row count of one or more tables.
    Count {
        /// Tables to count. Defaults to the market + custom cache tables.
        #[arg(long = "table")]
        tables: Vec<String>,
    },
    /// Run a SQL query against a table and print the result as CSV.
    Query {
        /// Table to register (under its own name) and query.
        #[arg(long)]
        table: String,
        /// SQL to execute (reference the table by name).
        #[arg(long)]
        sql: String,
    },
    /// Drop and recreate one or more tables as empty (purge all rows).
    ///
    /// Use to evict a poisoned cache table so the framework re-derives it from
    /// the history provider on the next run (e.g. rebuilding factor files after
    /// a provider bug fix).
    Reset {
        /// Tables to reset (drop + recreate empty).
        #[arg(long = "table", required = true)]
        tables: Vec<String>,
    },
}

const DEFAULT_TABLES: &[&str] = &[
    "market_trade_bars",
    "market_quote_bars",
    "market_ticks",
    "custom_points",
    "option_eod_bars",
    "option_universe",
    "factor_files",
    "map_files",
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Report => report(&args.data),
        Command::Compact { tables } => compact(&args.data, tables).await,
        Command::Restore { tables } => restore(&args.data, tables).await,
        Command::Count { tables } => count(&args.data, tables).await,
        Command::Query { table, sql } => query(&args.data, &table, &sql).await,
        Command::Reset { tables } => reset(&args.data, tables).await,
    }
}

async fn reset(data_root: &Path, tables: Vec<String>) -> Result<()> {
    let store = IcebergStore::connect_local(data_root).await?;
    for table in &tables {
        println!("resetting {table} (drop + recreate empty) ...");
        store
            .reset_table(table)
            .await
            .with_context(|| format!("failed to reset {table}"))?;
        let rows = store.count_rows(table).await.unwrap_or(0);
        println!("  {table} rows={rows}");
    }
    println!("done");
    Ok(())
}

async fn query(data_root: &Path, table: &str, sql: &str) -> Result<()> {
    use arrow_cast::display::{ArrayFormatter, FormatOptions};

    let store = IcebergStore::connect_local(data_root).await?;
    let batches = store
        .query_table(table, sql)
        .await
        .with_context(|| format!("failed to query {table}"))?;
    let batches: Vec<_> = batches.into_iter().filter(|b| b.num_rows() > 0).collect();
    if batches.is_empty() {
        eprintln!("(0 rows)");
        return Ok(());
    }

    // Tab-separated output so JSON payloads (which contain commas) survive.
    let schema = batches[0].schema();
    let header = schema
        .fields()
        .iter()
        .map(|field| field.name().clone())
        .collect::<Vec<_>>()
        .join("\t");
    println!("{header}");

    let options = FormatOptions::default();
    for batch in &batches {
        let formatters = batch
            .columns()
            .iter()
            .map(|column| ArrayFormatter::try_new(column.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()?;
        for row in 0..batch.num_rows() {
            let line = formatters
                .iter()
                .map(|formatter| formatter.value(row).to_string())
                .collect::<Vec<_>>()
                .join("\t");
            println!("{line}");
        }
    }
    Ok(())
}

async fn count(data_root: &Path, tables: Vec<String>) -> Result<()> {
    let store = IcebergStore::connect_local(data_root).await?;
    let tables: Vec<String> = if tables.is_empty() {
        DEFAULT_TABLES
            .iter()
            .map(|table| table.to_string())
            .collect()
    } else {
        tables
    };
    for table in &tables {
        let rows = store
            .count_rows(table)
            .await
            .with_context(|| format!("failed to count {table}"))?;
        println!("{table}: rows={rows}");
    }
    Ok(())
}

fn report(data_root: &Path) -> Result<()> {
    let metadata_root = data_root.join("iceberg").join("lean");
    println!("warehouse={}", data_root.join("iceberg").display());
    for table in DEFAULT_TABLES {
        let table_dir = metadata_root.join(table);
        if !table_dir.exists() {
            continue;
        }
        let metadata = table_dir.join("metadata");
        let data = table_dir.join("data");
        println!(
            "{table}: data_parquet={} metadata_json={} manifest_avro={}",
            count_extension(&data, "parquet")?,
            count_extension(&metadata, "json")?,
            count_extension(&metadata, "avro")?,
        );
    }
    Ok(())
}

async fn compact(data_root: &Path, tables: Vec<String>) -> Result<()> {
    let store = IcebergStore::connect_local(data_root).await?;
    let tables: Vec<String> = if tables.is_empty() {
        DEFAULT_TABLES
            .iter()
            .map(|table| table.to_string())
            .collect()
    } else {
        tables
    };
    for table in &tables {
        println!("compacting {table} ...");
        let started = std::time::Instant::now();
        let stats = store
            .compact_table(table)
            .await
            .with_context(|| format!("failed to compact {table}"))?;
        println!(
            "  {} rows={} live_files_before={} appends={} ({:.1}s)",
            stats.table,
            stats.rows,
            stats.files_before,
            stats.appends,
            started.elapsed().as_secs_f64(),
        );
    }
    println!("done");
    Ok(())
}

async fn restore(data_root: &Path, tables: Vec<String>) -> Result<()> {
    let store = IcebergStore::connect_local(data_root).await?;
    for table in &tables {
        println!("restoring {table} from staging ...");
        let started = std::time::Instant::now();
        let stats = store
            .restore_from_staging(table)
            .await
            .with_context(|| format!("failed to restore {table}"))?;
        println!(
            "  {} appends={} ({:.1}s)",
            stats.table,
            stats.appends,
            started.elapsed().as_secs_f64(),
        );
    }
    println!("done");
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
