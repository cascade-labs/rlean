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

    let plugin_cfgs = config::PluginConfigs::load().unwrap_or_default();
    let thetadata_cfg = plugin_cfgs.get_plugin("thetadata");
    let thetadata_api_key = thetadata_cfg.get("api_key")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    let parse_date = |s: &str| -> Result<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid date '{}', expected YYYY-MM-DD", s))
    };
    let start_date_override = args.start_date.as_deref().map(parse_date).transpose()?;
    let end_date_override   = args.end_date.as_deref().map(parse_date).transpose()?;

    // Determine report path: explicit --report flag, or auto-derive from strategy location.
    // Strategy lives at <project>/main.py → report goes to <project>/backtests/<timestamp>.html
    let report_path: PathBuf = if let Some(p) = args.report.clone() {
        p
    } else {
        let backtests_dir = args.strategy
            .parent()           // <project>/
            .map(|p| p.join("backtests"))
            .unwrap_or_else(|| PathBuf::from("backtests"));
        std::fs::create_dir_all(&backtests_dir)?;
        let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        backtests_dir.join(format!("{ts}.html"))
    };

    let config = RunConfig {
        data_root: args.data.clone(),
        historical_provider,
        thetadata_api_key,
        thetadata_rps: args.thetadata_rate,
        thetadata_concurrent: args.thetadata_concurrent,
        start_date_override,
        end_date_override,
        ..Default::default()
    };

    let results = run_strategy(&args.strategy, config).await?;

    results.print_summary();
    match write_report(&results, &report_path) {
        Ok(()) => println!("Report: {}", report_path.display()),
        Err(e) => eprintln!("Failed to write report: {e}"),
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

    let provider_args = providers::ProviderArgs {
        data_root:            args.data.clone(),
        polygon_rate:         args.polygon_rate,
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
