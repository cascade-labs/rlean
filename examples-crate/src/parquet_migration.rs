/// Migrate a LEAN CSV data directory to Parquet format.
/// Run this once against your existing data/ folder.
///
/// Usage:
///   cargo run --example parquet_migration -- --lean-data ./data --out ./data-parquet
use lean_core::{Market, Resolution, Symbol};
use lean_storage::{LeanCsvReader, WriterConfig, ParquetWriter, path_resolver::PathResolver};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let args: Vec<String> = std::env::args().collect();
    let lean_root = args.get(2).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("data"));
    let out_root  = args.get(4).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("data-parquet"));

    println!("Migrating {} -> {}", lean_root.display(), out_root.display());

    // Example: migrate SPY daily equity data
    let market = Market::usa();
    let symbol = Symbol::create_equity("SPY", &market);
    let resolution = Resolution::Daily;

    match LeanCsvReader::migrate_directory(&lean_root, &out_root, &symbol, resolution).await {
        Ok(n) => println!("Migrated {} bars for {}", n, symbol),
        Err(e) => eprintln!("Migration failed: {}", e),
    }

    // Example: verify round-trip read
    use lean_storage::{ParquetReader, QueryParams};
    let reader = ParquetReader::new();
    let resolver = PathResolver::new(&out_root);

    let today = chrono::Local::now().date_naive();
    let path = resolver.trade_bar(&symbol, resolution, today).to_path();

    if path.exists() {
        use lean_core::NanosecondTimestamp;
        use chrono::{TimeZone, Utc};
        let start = NanosecondTimestamp::from(Utc.from_utc_datetime(
            &today.and_hms_opt(0, 0, 0).unwrap()
        ));
        let end = NanosecondTimestamp::from(Utc.from_utc_datetime(
            &today.and_hms_opt(23, 59, 59).unwrap()
        ));

        let params = QueryParams::new().with_time_range(start, end);
        match reader.read_trade_bars(&[path], symbol.clone(), &params).await {
            Ok(bars) => println!("Read {} bars back from parquet", bars.len()),
            Err(e) => eprintln!("Read failed: {}", e),
        }
    }
}
