/// rlean — unified Lean-Rust execution CLI
///
/// Usage:
///   rlean init                                   # bootstrap workspace
///   rlean create-project <name>                  # scaffold a new strategy project
///   rlean backtest <strategy> [OPTIONS]           # run a backtest
///   rlean live     <strategy> [OPTIONS]           # run live trading
///   rlean research <project> [OPTIONS]            # launch Jupyter research session
///
/// Strategy types (auto-detected by file extension):
///   .py             Python strategy (AlgorithmImports / QCAlgorithm)
///   .so / .dylib    Compiled Rust strategy plugin (exports `create_algorithm`)
///
/// Examples:
///   rlean init
///   rlean create-project my_strategy
///   rlean backtest my_strategy/main.py --thetadata-api-key $THETADATA_API_KEY
///   rlean research my_strategy
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use lean_data::IHistoricalDataProvider;
use lean_data_providers::IHistoryProvider;

mod config;
mod config_cmd;
mod init;
mod plugin_cmd;
mod project;
mod providers;
mod research;
mod stubs_cmd;

use config_cmd::{ConfigArgs, run_config};
use init::{InitArgs, run_init};
use plugin_cmd::{PluginArgs, run_plugin};
use project::{CreateProjectArgs, run_create_project};
use research::{ResearchArgs, run_research};
use stubs_cmd::{StubsArgs, run_stubs};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "rlean",
    about = "Lean-Rust backtest, live trading, and research runner",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bootstrap a Lean workspace in the current directory (creates rlean.json and data/)
    Init(InitArgs),

    /// Scaffold a new strategy project
    #[command(name = "create-project")]
    CreateProject(CreateProjectArgs),

    /// Get, set, or list configuration values (API keys, language, data-folder)
    Config(ConfigArgs),

    /// Manage rlean plugins (brokerages, data providers, AI skills, custom data)
    Plugin(PluginArgs),

    /// Run a backtest
    Backtest(RunArgs),

    /// Run live trading
    Live(RunArgs),

    /// Launch an interactive research session for a project (opens research.ipynb)
    Research(ResearchArgs),

    /// Generate and install AlgorithmImports.pyi stub files for IDE autocomplete
    Stubs(StubsArgs),
}

#[derive(clap::Args, Clone)]
struct RunArgs {
    /// Path to the strategy file (.py) or compiled plugin (.so/.dylib)
    strategy: PathBuf,

    // ── Data ─────────────────────────────────────────────────────────────────
    /// Parquet data root directory
    #[arg(long, default_value = "data", env = "RLEAN_DATA")]
    data: PathBuf,

    /// Comma-separated provider priority list (e.g. thetadata,polygon)
    #[arg(long, env = "RLEAN_DATA_PROVIDER_HISTORICAL")]
    data_provider_historical: Option<String>,

    /// Live data provider (polygon | thetadata) — live trading only
    #[arg(long, env = "RLEAN_DATA_PROVIDER_LIVE")]
    data_provider_live: Option<String>,

    // ── Date range override ───────────────────────────────────────────────────
    /// Override the strategy start date (YYYY-MM-DD)
    #[arg(long)]
    start_date: Option<String>,

    /// Override the strategy end date (YYYY-MM-DD)
    #[arg(long)]
    end_date: Option<String>,

    // ── Rate limits (plugin API keys/URLs live in ~/.rlean/plugin-configs.json) ─
    /// Polygon/Massive requests/second (default: 5)
    #[arg(long, default_value_t = 5.0)]
    polygon_rate: f64,

    /// ThetaData requests/second (default: 4)
    #[arg(long, default_value_t = 4.0)]
    thetadata_rate: f64,

    /// ThetaData max concurrent requests (default: 4)
    #[arg(long, default_value_t = 4)]
    thetadata_concurrent: usize,

    // ── Output ────────────────────────────────────────────────────────────────
    /// Override the report output path (default: <project>/backtests/<timestamp>.html)
    #[arg(long)]
    report: Option<PathBuf>,

    // ── Logging ───────────────────────────────────────────────────────────────
    /// Enable debug logging (equivalent to RUST_LOG=debug)
    #[arg(long, short = 'v')]
    verbose: bool,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let verbose = match &cli.command {
        Command::Backtest(args) | Command::Live(args) => args.verbose,
        _ => false,
    };

    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::from_default_env()
            .add_directive("info".parse().unwrap())
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .init();

    match cli.command {
        Command::Init(args)          => run_init(args),
        Command::CreateProject(args) => run_create_project(args),
        Command::Config(args)        => run_config(args),
        Command::Plugin(args)        => run_plugin(args),
        Command::Backtest(args)      => run_backtest(args).await,
        Command::Live(args)          => run_live(args).await,
        Command::Research(args)      => run_research(args),
        Command::Stubs(args)         => run_stubs(args),
    }
}

// ── Backtest ──────────────────────────────────────────────────────────────────

async fn run_backtest(mut args: RunArgs) -> Result<()> {
    // If the user passed a directory, look for main.py inside it.
    if args.strategy.is_dir() {
        let candidate = args.strategy.join("main.py");
        if candidate.exists() {
            args.strategy = candidate;
        } else {
            bail!(
                "'{}' is a directory but contains no main.py. \
                 Pass the strategy file directly or run `rlean create-project` to scaffold one.",
                args.strategy.display()
            );
        }
    }

    validate_strategy_path(&args.strategy)?;

    let (historical_provider, history_provider) = build_providers(&args)?;

    let ext = args.strategy.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "py" => run_python_backtest(args, historical_provider, history_provider).await,
        "so" | "dylib" => run_rust_plugin_backtest(args),
        other => bail!(
            "Unknown strategy extension '.{}'. Expected .py, .so, or .dylib",
            other
        ),
    }
}

async fn run_python_backtest(
    args: RunArgs,
    historical_provider: Option<Arc<dyn IHistoricalDataProvider>>,
    history_provider: Option<Arc<dyn IHistoryProvider>>,
) -> Result<()> {
    use lean_python::AlgorithmImports;
    use lean_python::runner::{run_strategy, RunConfig};
    use lean_python::report::{
        write_report, write_results_json, write_order_events_json,
        write_summary_json, write_log_txt, write_data_request_files,
    };

    // Register the AlgorithmImports PyO3 module before starting Python.
    // Must be called before prepare_freethreaded_python.
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::prepare_freethreaded_python();


    let parse_date = |s: &str| -> Result<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid date '{}', expected YYYY-MM-DD", s))
    };
    let start_date_override = args.start_date.as_deref().map(parse_date).transpose()?;
    let end_date_override   = args.end_date.as_deref().map(parse_date).transpose()?;

    // Each backtest creates a LEAN-compatible output directory:
    //   <project>/backtests/YYYY-MM-DD_<strategy-name>/
    //
    // Matches C# LEAN format (e.g. "backtests/2026-04-01_sma_crossover/").
    // When --report is set it is treated as the folder path directly.
    let backtest_dir: PathBuf = if let Some(p) = args.report.clone() {
        p
    } else {
        let backtests_root = args.strategy
            .parent()           // <project>/
            .map(|p| p.join("backtests"))
            .unwrap_or_else(|| PathBuf::from("backtests"));
        let now  = chrono::Utc::now();
        let name = strategy_name_from_path(&args.strategy);
        backtests_root.join(backtest_dir_name(now, &name))
    };
    std::fs::create_dir_all(&backtest_dir)?;

    let config = RunConfig {
        data_root: args.data.clone(),
        historical_provider,
        history_provider,
        start_date_override,
        end_date_override,
        ..Default::default()
    };

    let results = run_strategy(&args.strategy, config).await?;

    results.print_summary();

    // The backtest ID (Unix epoch seconds at backtest start) is used as the
    // filename prefix for all per-backtest files, matching C# LEAN's convention.
    let id = results.backtest_id;
    // Millisecond timestamp suffix for data-request files.
    let ts_ms = chrono::Utc::now().format("%Y%m%d%H%M%S%3f");

    // ── write all output files ────────────────────────────────────────────────
    let json_path      = backtest_dir.join(format!("{id}.json"));
    let order_events_path = backtest_dir.join(format!("{id}-order-events.json"));
    let summary_path   = backtest_dir.join(format!("{id}-summary.json"));
    let id_log_path    = backtest_dir.join(format!("{id}-log.txt"));
    let top_log_path   = backtest_dir.join("log.txt");
    let succeeded_path = backtest_dir.join(format!("succeeded-data-requests-{ts_ms}.txt"));
    let failed_path    = backtest_dir.join(format!("failed-data-requests-{ts_ms}.txt"));
    let report_path    = backtest_dir.join("report.html");

    if let Err(e) = write_results_json(&results, &json_path)           { eprintln!("Failed to write results: {e}"); }
    if let Err(e) = write_order_events_json(&results, &order_events_path) { eprintln!("Failed to write order events: {e}"); }
    if let Err(e) = write_summary_json(&results, &summary_path)        { eprintln!("Failed to write summary: {e}"); }
    if let Err(e) = write_log_txt(&results, &id_log_path)              { eprintln!("Failed to write log: {e}"); }
    let _ = std::fs::copy(&id_log_path, &top_log_path);
    if let Err(e) = write_data_request_files(&results, &succeeded_path, &failed_path) { eprintln!("Failed to write data requests: {e}"); }
    if let Err(e) = write_report(&results, &report_path)               { eprintln!("Failed to write report: {e}"); }

    println!("Results: {}", backtest_dir.display());
    Ok(())
}

fn run_rust_plugin_backtest(args: RunArgs) -> Result<()> {
    use lean_engine::{BacktestEngine, EngineConfig};
    use lean_algorithm::algorithm::IAlgorithm;
    use libloading::{Library, Symbol};

    // Safety: the plugin must export `create_algorithm` with C ABI.
    let lib = unsafe { Library::new(&args.strategy) }
        .map_err(|e| anyhow::anyhow!("Failed to load plugin '{}': {e}", args.strategy.display()))?;

    let create: Symbol<unsafe extern "C" fn() -> Box<dyn IAlgorithm>> =
        unsafe { lib.get(b"create_algorithm\0") }
            .map_err(|_| anyhow::anyhow!(
                "Plugin does not export `create_algorithm`. \
                 Add `#[no_mangle] pub extern \"C\" fn create_algorithm() -> Box<dyn IAlgorithm>` to your strategy crate."
            ))?;

    let algo = unsafe { create() };

    let config = EngineConfig {
        data_root: args.data,
        ..Default::default()
    };

    let engine = BacktestEngine::new(config);
    // block_in_place lets us call async code from a sync fn inside an async context.
    match tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(engine.run(algo))
    }) {
        Ok(results) => results.print_summary(),
        Err(e) => {
            eprintln!("Backtest failed: {e}");
            std::process::exit(1);
        }
    }

    drop(lib);
    Ok(())
}

// ── Live ──────────────────────────────────────────────────────────────────────

async fn run_live(_args: RunArgs) -> Result<()> {
    bail!("Live trading not yet implemented. Coming soon.")
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Derive a human-readable strategy name from a strategy file path.
///
/// Rules (matching C# LEAN's project-name convention):
///  - If the file is `main.py`, use the parent directory name.
///  - Otherwise use the file stem (filename without extension).
///  - Falls back to `"strategy"` when neither can be determined.
///
/// Examples:
///  - `sma_crossover/main.py`     → `"sma_crossover"`
///  - `my_algo/my_algo.py`        → `"my_algo"`
///  - `/absolute/path/signal.py`  → `"signal"`
pub(crate) fn strategy_name_from_path(strategy: &std::path::Path) -> String {
    let stem = strategy.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("strategy")
        .to_string();
    if stem == "main" {
        strategy
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("strategy")
            .to_string()
    } else {
        stem
    }
}

/// Build the backtest output directory path in LEAN format:
///   `<backtests_root>/YYYY-MM-DD_HHMMSS_<strategy_name>`
pub(crate) fn backtest_dir_name(datetime: chrono::DateTime<chrono::Utc>, strategy_name: &str) -> String {
    format!("{}_{}", datetime.format("%Y-%m-%d_%H%M%S"), strategy_name)
}

fn validate_strategy_path(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        bail!("Strategy file not found: {}", path.display());
    }
    Ok(())
}

fn build_providers(args: &RunArgs) -> Result<(
    Option<Arc<dyn IHistoricalDataProvider>>,
    Option<Arc<dyn IHistoryProvider>>,
)> {
    let names = match args.data_provider_historical.as_deref() {
        Some(n) => n,
        None => return Ok((None, None)),
    };

    let provider_args = providers::ProviderArgs {
        data_root:            args.data.clone(),
        polygon_rate:         args.polygon_rate,
        thetadata_rate:       args.thetadata_rate,
        thetadata_concurrent: args.thetadata_concurrent,
    };

    let raw = providers::build_history_provider(names, provider_args)?;
    let historical = Arc::new(HistoryProviderAdapter(Arc::clone(&raw)));
    Ok((Some(historical), Some(raw)))
}

// ─── Adapter: IHistoryProvider → IHistoricalDataProvider ─────────────────────

struct HistoryProviderAdapter(Arc<dyn IHistoryProvider>);

impl IHistoricalDataProvider for HistoryProviderAdapter {
    fn get_trade_bars(
        &self,
        symbol: lean_core::Symbol,
        resolution: lean_core::Resolution,
        start: lean_core::DateTime,
        end: lean_core::DateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = lean_core::Result<Vec<lean_data::TradeBar>>> + Send + '_>>
    {
        let provider = Arc::clone(&self.0);
        let request = lean_data_providers::HistoryRequest {
            symbol:     symbol.clone(),
            resolution,
            start,
            end,
            data_type:  lean_data_providers::DataType::TradeBar,
        };
        // IHistoryProvider::get_history is synchronous so that plugins (cdylibs
        // with their own tokio copy) can block internally without conflicting
        // with the host runtime's thread-locals.  We bridge to async here using
        // spawn_blocking so the tokio scheduler is not blocked.
        Box::pin(async move {
            tokio::task::spawn_blocking(move || provider.get_history(&request))
                .await
                .map_err(|e| lean_core::LeanError::DataError(format!("provider task panicked: {e}")))?
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ── strategy_name_from_path ────────────────────────────────────────────────

    #[test]
    fn test_strategy_name_main_py_uses_parent_dir() {
        // "sma_crossover/main.py" → "sma_crossover"
        let p = Path::new("sma_crossover/main.py");
        assert_eq!(strategy_name_from_path(p), "sma_crossover");
    }

    #[test]
    fn test_strategy_name_non_main_uses_stem() {
        // "sma_crossover/my_algo.py" → "my_algo"
        let p = Path::new("sma_crossover/my_algo.py");
        assert_eq!(strategy_name_from_path(p), "my_algo");
    }

    #[test]
    fn test_strategy_name_absolute_path_main_py() {
        let p = Path::new("/home/user/strategies/etf_blend/main.py");
        assert_eq!(strategy_name_from_path(p), "etf_blend");
    }

    #[test]
    fn test_strategy_name_absolute_path_named_file() {
        let p = Path::new("/home/user/strategies/signal_generator.py");
        assert_eq!(strategy_name_from_path(p), "signal_generator");
    }

    #[test]
    fn test_strategy_name_rust_plugin() {
        // Rust plugins use .so/.dylib extensions — stem is used directly.
        let p = Path::new("plugins/my_strategy.so");
        assert_eq!(strategy_name_from_path(p), "my_strategy");
    }

    // ── backtest_dir_name ──────────────────────────────────────────────────────

    #[test]
    fn test_backtest_dir_name_format() {
        use chrono::{TimeZone, Utc};
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 0).unwrap();
        let dir = backtest_dir_name(dt, "sma_crossover");
        assert_eq!(dir, "2026-04-10_143000_sma_crossover");
    }

    #[test]
    fn test_backtest_dir_name_seconds_unique() {
        use chrono::{TimeZone, Utc};
        let dt1 = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 0).unwrap();
        let dt2 = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 5).unwrap();
        let d1  = backtest_dir_name(dt1, "spy_wheel");
        let d2  = backtest_dir_name(dt2, "spy_wheel");
        assert_ne!(d1, d2, "runs on same day must produce different dirs");
    }

    #[test]
    fn test_backtest_dir_name_date_prefix() {
        use chrono::{TimeZone, Utc};
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 9, 5, 3).unwrap();
        let dir = backtest_dir_name(dt, "sma_crossover");
        assert!(dir.starts_with("2026-04-10_090503_"), "dir={dir}");
    }
}
