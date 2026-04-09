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
    /// Bootstrap a Lean workspace in the current directory (creates lean.json and data/)
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

    // ── Polygon ───────────────────────────────────────────────────────────────
    /// Polygon.io API key (or POLYGON_API_KEY env)
    #[arg(long, env = "POLYGON_API_KEY")]
    polygon_api_key: Option<String>,

    /// Polygon requests/second (default: 5)
    #[arg(long, default_value_t = 5.0)]
    polygon_rate: f64,

    // ── ThetaData ─────────────────────────────────────────────────────────────
    /// ThetaData API key (or THETADATA_API_KEY env)
    #[arg(long, env = "THETADATA_API_KEY")]
    thetadata_api_key: Option<String>,

    /// ThetaData requests/second (default: 4)
    #[arg(long, default_value_t = 4.0)]
    thetadata_rate: f64,

    /// ThetaData max concurrent requests (default: 4)
    #[arg(long, default_value_t = 4)]
    thetadata_concurrent: usize,

    // ── Output ────────────────────────────────────────────────────────────────
    /// Write an HTML performance report to this path
    #[arg(long)]
    report: Option<PathBuf>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("info".parse().unwrap()),
        )
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

    let historical_provider = build_historical_provider(&args)?;

    let ext = args.strategy.extension().and_then(|e| e.to_str()).unwrap_or("");

    match ext {
        "py" => run_python_backtest(args, historical_provider).await,
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
) -> Result<()> {
    use lean_python::AlgorithmImports;
    use lean_python::runner::{run_strategy, RunConfig};
    use lean_python::report::write_report;

    // Register the AlgorithmImports PyO3 module before starting Python.
    // Must be called before prepare_freethreaded_python.
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::prepare_freethreaded_python();

    let creds = config::Credentials::load().unwrap_or_default();
    let thetadata_api_key = args.thetadata_api_key.clone()
        .or_else(|| creds.thetadata_api_key);

    let strategy = args.strategy.clone();
    let report = args.report.clone();
    let config = RunConfig {
        data_root: args.data.clone(),
        historical_provider,
        thetadata_api_key,
        thetadata_rps: args.thetadata_rate,
        thetadata_concurrent: args.thetadata_concurrent,
        ..Default::default()
    };

    // run_strategy is synchronous and creates its own tokio runtime internally.
    // Use spawn_blocking so it runs on a thread without an active tokio context,
    // avoiding the "cannot start a runtime from within a runtime" panic.
    let results = tokio::task::spawn_blocking(move || run_strategy(&strategy, config))
        .await
        .map_err(|e| anyhow::anyhow!("Strategy thread panicked: {e}"))??;

    results.print_summary();
    if let Some(ref path) = report {
        match write_report(&results, path) {
            Ok(()) => println!("Report written to {}", path.display()),
            Err(e) => eprintln!("Failed to write report: {e}"),
        }
    }

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

fn validate_strategy_path(path: &PathBuf) -> Result<()> {
    if !path.exists() {
        bail!("Strategy file not found: {}", path.display());
    }
    Ok(())
}

fn build_historical_provider(args: &RunArgs) -> Result<Option<Arc<dyn IHistoricalDataProvider>>> {
    let names = match args.data_provider_historical.as_deref() {
        Some(n) => n,
        None => return Ok(None),
    };

    // API keys: CLI flag > env var (already merged by clap) > ~/.rlean/credentials
    let creds = config::Credentials::load().unwrap_or_default();
    let polygon_api_key = args.polygon_api_key.clone()
        .or_else(|| creds.polygon_api_key.clone());
    let thetadata_api_key = args.thetadata_api_key.clone()
        .or_else(|| creds.thetadata_api_key.clone());

    let provider_args = providers::ProviderArgs {
        data_root:            args.data.clone(),
        polygon_api_key,
        polygon_rate:         args.polygon_rate,
        thetadata_api_key,
        thetadata_rate:       args.thetadata_rate,
        thetadata_concurrent: args.thetadata_concurrent,
    };

    let new_provider = providers::build_history_provider(names, provider_args)?;
    Ok(Some(Arc::new(HistoryProviderAdapter(new_provider))))
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
        Box::pin(async move {
            provider
                .get_history(&request)
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))
        })
    }
}
