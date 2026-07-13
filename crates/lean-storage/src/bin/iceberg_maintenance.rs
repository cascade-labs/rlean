use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use lean_storage::{IcebergStore, RestCatalogConfig, SigV4Config, DEFAULT_NAMESPACE};

#[derive(Parser)]
#[command(about = "Inspect and maintain rlean Iceberg cache tables (REST catalog)")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the live row count and total for each cache table.
    Report,
    /// Print the live row count of one or more tables.
    Count {
        /// Tables to count. Defaults to the market + custom cache tables.
        #[arg(long = "table")]
        tables: Vec<String>,
    },
    /// Run a SQL query against a table and print the result as tab-separated
    /// rows.
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
    /// a provider bug fix). Drop+recreate is safe on the REST catalog.
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

/// Build the REST catalog connection from environment variables. Config
/// resolution proper lives in the `rlean` crate; the maintenance binary reads
/// the same connection directly so it needs no cross-crate dependency.
///
/// - `RLEAN_DATA_CATALOG`     REST catalog base URI (required)
/// - `RLEAN_DATA_WAREHOUSE`   warehouse identifier / table-bucket ARN (required)
/// - `RLEAN_DATA_SIGV4_REGION` SigV4 signing region; when set, requests are
///   signed with SigV4
/// - `RLEAN_DATA_SIGV4_NAME`  SigV4 signing name (defaults to `s3tables` when a
///   region is set)
/// - `RLEAN_DATA_NAMESPACE`   Iceberg namespace holding the cache tables
///   (defaults to `lean`)
fn config_from_env() -> Result<RestCatalogConfig> {
    let uri = std::env::var("RLEAN_DATA_CATALOG")
        .context("RLEAN_DATA_CATALOG must be set to the REST catalog base URI")?;
    let warehouse = std::env::var("RLEAN_DATA_WAREHOUSE")
        .context("RLEAN_DATA_WAREHOUSE must be set to the warehouse identifier")?;
    let namespace = std::env::var("RLEAN_DATA_NAMESPACE")
        .ok()
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
    let sigv4 = match std::env::var("RLEAN_DATA_SIGV4_REGION") {
        Ok(region) if !region.is_empty() => {
            let signing_name = std::env::var("RLEAN_DATA_SIGV4_NAME")
                .ok()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "s3tables".to_string());
            Some(SigV4Config {
                region,
                signing_name,
            })
        }
        _ => None,
    };
    Ok(RestCatalogConfig {
        uri,
        warehouse,
        sigv4,
        namespace,
    })
}

async fn connect() -> Result<IcebergStore> {
    IcebergStore::connect(config_from_env()?)
        .await
        .context("failed to connect to the REST Iceberg catalog")
}

fn resolve_tables(tables: Vec<String>) -> Vec<String> {
    if tables.is_empty() {
        DEFAULT_TABLES
            .iter()
            .map(|table| table.to_string())
            .collect()
    } else {
        tables
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Report => report().await,
        Command::Count { tables } => count(tables).await,
        Command::Query { table, sql } => query(&table, &sql).await,
        Command::Reset { tables } => reset(tables).await,
    }
}

async fn reset(tables: Vec<String>) -> Result<()> {
    let store = connect().await?;
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

async fn query(table: &str, sql: &str) -> Result<()> {
    use arrow_cast::display::{ArrayFormatter, FormatOptions};

    let store = connect().await?;
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

async fn count(tables: Vec<String>) -> Result<()> {
    let store = connect().await?;
    for table in &resolve_tables(tables) {
        let rows = store
            .count_rows(table)
            .await
            .with_context(|| format!("failed to count {table}"))?;
        println!("{table}: rows={rows}");
    }
    Ok(())
}

async fn report() -> Result<()> {
    let store = connect().await?;
    let mut total = 0usize;
    for table in DEFAULT_TABLES {
        let rows = store
            .count_rows(table)
            .await
            .with_context(|| format!("failed to count {table}"))?;
        total += rows;
        println!("{table}: rows={rows}");
    }
    println!("total: rows={total}");
    Ok(())
}
