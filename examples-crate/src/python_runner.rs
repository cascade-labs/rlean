/// lean-python runner binary.
///
/// Usage:
///   cargo run --bin python_runner -- path/to/strategy.py [OPTIONS]
///
/// Options:
///   --data <dir>                          Local data directory (default: data/)
///   --data-provider-historical <name>     Fetch missing data (massive | thetadata | local)
///   --polygon-api-key <key>               Massive/Polygon API key (or POLYGON_API_KEY)
///   --polygon-rate <rps>                  Requests/sec for Massive (default: 5.0)
///   --thetadata-api-key <key>             ThetaData API key (or THETADATA_API_KEY)
///   --thetadata-rate <rps>                Requests/sec for ThetaData (default: 4.0)
///   --thetadata-concurrent <n>            Max concurrent ThetaData requests (default: 4)
///
/// The strategy file must contain a class that inherits from
/// `AlgorithmImports.QCAlgorithm` (i.e., `from AlgorithmImports import *; class MyAlgo(QCAlgorithm)`).
///
/// NOTE: Data providers (massive, thetadata) are loaded as plugins from
/// ~/.rlean/plugins/ at runtime.  Install them with `rlean plugin install <name>`.
use std::path::PathBuf;

use lean_data::IHistoricalDataProvider;
use lean_python::report::write_report;
use lean_python::runner::{run_strategy, RunConfig};
use lean_python::AlgorithmImports;  // the #[pymodule] fn — needed for append_to_inittab!
use tracing_subscriber::EnvFilter;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "Usage: python_runner <strategy.py> [--data <dir>] \
             [--data-provider-historical massive|thetadata|local] [--polygon-api-key <key>] \
             [--polygon-rate <rps>]"
        );
        std::process::exit(1);
    }

    let strategy_path = PathBuf::from(&args[1]);
    if !strategy_path.exists() {
        eprintln!("Error: strategy file not found: {}", strategy_path.display());
        std::process::exit(1);
    }

    // ── parse CLI args ───────────────────────────────────────────────────────
    let data_root = find_arg(&args, "--data")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));

    let _provider_name = find_arg(&args, "--data-provider-historical")
        .unwrap_or_default();

    // Providers are now loaded as plugins via rlean plugin install.
    // For backwards compatibility, we accept the old flags but always use local.
    let historical_provider: Option<std::sync::Arc<dyn IHistoricalDataProvider>> = None;

    // ── logging ──────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap()),
        )
        .init();

    // ── CRITICAL: register AlgorithmImports module BEFORE starting Python ────
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::prepare_freethreaded_python();

    // ── ThetaData API key (for real option EOD chains) ────────────────────────
    let thetadata_api_key = find_arg(&args, "--thetadata-api-key")
        .or_else(|| std::env::var("THETADATA_API_KEY").ok());

    let thetadata_rps: f64 = find_arg(&args, "--thetadata-rate")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4.0);

    let thetadata_concurrent: usize = find_arg(&args, "--thetadata-concurrent")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    // ── run backtest ─────────────────────────────────────────────────────────
    let config = RunConfig {
        data_root,
        historical_provider,
        thetadata_api_key,
        thetadata_rps,
        thetadata_concurrent,
        ..Default::default()
    };
    let report_path = find_arg(&args, "--report").map(std::path::PathBuf::from);

    match run_strategy(&strategy_path, config) {
        Ok(results) => {
            results.print_summary();
            if let Some(ref path) = report_path {
                match write_report(&results, path) {
                    Ok(()) => println!("Report written to {}", path.display()),
                    Err(e) => eprintln!("Failed to write report: {}", e),
                }
            }
        }
        Err(e) => {
            eprintln!("Backtest failed: {:#}", e);
            std::process::exit(1);
        }
    }
}

/// Find the value that follows a flag in the args list.
fn find_arg(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}
