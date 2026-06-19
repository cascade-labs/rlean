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
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

use lean_algorithm::qc_algorithm::BrokerageName;
use lean_data::IHistoricalDataProvider;
use lean_data_providers::IHistoryProvider;

mod config;
mod config_cmd;
mod init;
mod plugin_cmd;
mod project;
mod providers;
mod registry_cmd;
mod research;
mod research_daemon;
mod stubs_cmd;
mod vcs_cmd;

use config_cmd::{run_config, ConfigArgs};
use init::{run_init, InitArgs};
use plugin_cmd::{run_plugin, PluginArgs};
use project::{run_create_project, CreateProjectArgs};
use registry_cmd::{run_registry, RegistryArgs};
use research::{run_research, ResearchArgs};
use research_daemon::{run_daemon, ResearchDaemonArgs};
use stubs_cmd::{run_stubs, StubsArgs};
use vcs_cmd::{run_vcs, VcsArgs};

type ProviderPair = (
    Option<Arc<dyn IHistoricalDataProvider>>,
    Option<Arc<dyn IHistoryProvider>>,
);

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

    /// Manage plugin registries (add, remove, list)
    Registry(RegistryArgs),

    /// Run a backtest
    Backtest(RunArgs),

    /// Run live trading or inspect local live deployments
    Live(LiveArgs),

    /// Launch an interactive research session for a project (opens research.ipynb)
    Research(ResearchArgs),

    /// Generate and install AlgorithmImports.pyi stub files for IDE autocomplete
    Stubs(StubsArgs),

    /// Version control: push, pull, sync, and configure your strategy remote
    Vcs(VcsArgs),

    /// Hidden: persistent PyO3 research kernel daemon (started by `rlean research`)
    #[command(name = "__research-daemon", hide = true)]
    ResearchDaemon(ResearchDaemonArgs),
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

    /// Live data provider plugin(s), comma-separated for stacked live feeds
    #[arg(long, env = "RLEAN_DATA_PROVIDER_LIVE")]
    data_provider_live: Option<String>,

    /// Brokerage for live runs. Use "paper" for simulated fills, or a real
    /// brokerage plugin name such as "tradier" or "hyperliquid" for live orders.
    #[arg(long, env = "RLEAN_BROKERAGE")]
    brokerage: Option<String>,

    // ── Date range override ───────────────────────────────────────────────────
    /// Override the strategy start date (YYYY-MM-DD)
    #[arg(long)]
    start_date: Option<String>,

    /// Override the strategy end date (YYYY-MM-DD)
    #[arg(long)]
    end_date: Option<String>,

    /// Algorithm parameter as KEY=VALUE for LEAN-style GetParameter access.
    #[arg(long = "parameter", short = 'p', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    parameters: Vec<String>,

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

    /// Stop a live run after N slices. Useful for smoke tests.
    #[arg(long, hide = true)]
    live_max_slices: Option<usize>,

    /// Stop a live run after this many wall-clock seconds. Useful for paper soaks.
    #[arg(long, hide = true)]
    live_max_runtime_seconds: Option<u64>,

    // ── Logging ───────────────────────────────────────────────────────────────
    /// Enable debug logging for rlean crates
    #[arg(long, short = 'v')]
    verbose: bool,
}

#[derive(clap::Args, Clone)]
struct LiveArgs {
    #[command(subcommand)]
    command: Option<LiveSubcommand>,

    /// Path to the strategy file (.py) or project directory containing main.py
    strategy: Option<PathBuf>,

    /// Parquet data root directory
    #[arg(long, default_value = "data", env = "RLEAN_DATA")]
    data: PathBuf,

    /// Comma-separated provider priority list (e.g. thetadata,polygon)
    #[arg(long, env = "RLEAN_DATA_PROVIDER_HISTORICAL")]
    data_provider_historical: Option<String>,

    /// Live data provider plugin(s), comma-separated for stacked live feeds
    #[arg(long, env = "RLEAN_DATA_PROVIDER_LIVE")]
    data_provider_live: Option<String>,

    /// Brokerage for live runs. Use "paper" for simulated fills, or a real
    /// brokerage plugin name such as "tradier" or "hyperliquid" for live orders.
    #[arg(long, env = "RLEAN_BROKERAGE")]
    brokerage: Option<String>,

    /// Override the strategy start date (YYYY-MM-DD)
    #[arg(long)]
    start_date: Option<String>,

    /// Override the strategy end date (YYYY-MM-DD)
    #[arg(long)]
    end_date: Option<String>,

    /// Algorithm parameter as KEY=VALUE for LEAN-style GetParameter access.
    #[arg(long = "parameter", short = 'p', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    parameters: Vec<String>,

    /// Polygon/Massive requests/second (default: 5)
    #[arg(long, default_value_t = 5.0)]
    polygon_rate: f64,

    /// ThetaData requests/second (default: 4)
    #[arg(long, default_value_t = 4.0)]
    thetadata_rate: f64,

    /// ThetaData max concurrent requests (default: 4)
    #[arg(long, default_value_t = 4)]
    thetadata_concurrent: usize,

    /// Override the live deployment directory.
    #[arg(long, hide = true)]
    live_deploy_dir: Option<PathBuf>,

    /// Run live trading in the foreground instead of creating a detached deployment.
    #[arg(long, hide = true)]
    foreground: bool,

    /// Stop a live run after N slices. Useful for smoke tests.
    #[arg(long, hide = true)]
    live_max_slices: Option<usize>,

    /// Stop a live run after this many wall-clock seconds. Useful for paper soaks.
    #[arg(long, hide = true)]
    live_max_runtime_seconds: Option<u64>,

    /// Enable debug logging for rlean crates
    #[arg(long, short = 'v')]
    verbose: bool,
}

#[derive(Subcommand, Clone)]
enum LiveSubcommand {
    /// List local live deployments
    List {
        /// Filter by deployment status
        #[arg(long, value_enum)]
        status: Option<LiveStatusFilter>,
    },
    /// Show one local live deployment status
    Status { deploy_id: String },
    /// Print the latest portfolio snapshot for a live deployment
    Portfolio { deploy_id: String },
    /// Print the latest orders snapshot for a live deployment
    Orders { deploy_id: String },
    /// Print live deployment logs
    Logs {
        deploy_id: String,
        /// Number of trailing log lines to print
        #[arg(long, default_value_t = 250)]
        lines: usize,
    },
    /// Pause a running local live deployment
    Pause {
        deploy_id: String,
        /// Seconds to wait for the process to exit after SIGTERM
        #[arg(long, default_value_t = 30)]
        timeout_seconds: u64,
    },
    /// Resume a paused or stopped local live deployment
    Resume { deploy_id: String },
    /// Upgrade the code snapshot for a paused live deployment
    Upgrade { deploy_id: String },
    /// Remove a local live deployment and its deployment directory
    Remove {
        deploy_id: String,
        /// Terminate and remove even if the deployment is running
        #[arg(long)]
        force: bool,
    },
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
#[clap(rename_all = "kebab-case")]
enum LiveStatusFilter {
    Running,
    Paused,
    Stopped,
    RuntimeError,
    Liquidated,
}

impl LiveArgs {
    fn to_run_args(&self) -> Result<RunArgs> {
        let strategy = self
            .strategy
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing strategy path"))?;
        Ok(RunArgs {
            strategy,
            data: self.data.clone(),
            data_provider_historical: self.data_provider_historical.clone(),
            data_provider_live: self.data_provider_live.clone(),
            brokerage: self.brokerage.clone(),
            start_date: self.start_date.clone(),
            end_date: self.end_date.clone(),
            parameters: self.parameters.clone(),
            polygon_rate: self.polygon_rate,
            thetadata_rate: self.thetadata_rate,
            thetadata_concurrent: self.thetadata_concurrent,
            report: None,
            live_max_slices: self.live_max_slices,
            live_max_runtime_seconds: self.live_max_runtime_seconds,
            verbose: self.verbose,
        })
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let verbose = match &cli.command {
        Command::Backtest(args) => args.verbose,
        Command::Live(args) => args.verbose,
        _ => false,
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            EnvFilter::new(
                "info,rlean=debug,lean_algorithm=debug,lean_core=debug,lean_data=debug,\
                 lean_data_providers=debug,lean_engine=debug,lean_python=debug,\
                 lean_storage=debug",
            )
        } else {
            EnvFilter::new("info")
        }
    });

    tracing_subscriber::fmt().with_env_filter(filter).init();

    match cli.command {
        Command::Init(args) => run_init(args),
        Command::CreateProject(args) => run_create_project(args),
        Command::Config(args) => run_config(args),
        Command::Plugin(args) => run_plugin(args),
        Command::Registry(args) => run_registry(args),
        Command::Backtest(args) => run_backtest(args).await,
        Command::Live(args) => run_live(args).await,
        Command::Research(args) => run_research(args),
        Command::Stubs(args) => run_stubs(args),
        Command::Vcs(args) => run_vcs(args),
        Command::ResearchDaemon(args) => run_daemon(args),
    }
}

fn parse_algorithm_parameters(raw: &[String]) -> Result<HashMap<String, String>> {
    let mut parameters = HashMap::new();
    for item in raw {
        let Some((key, value)) = item.split_once('=') else {
            bail!("invalid algorithm parameter '{item}', expected KEY=VALUE");
        };
        if key.is_empty() {
            bail!("invalid algorithm parameter '{item}', key cannot be empty");
        }
        parameters.insert(key.to_string(), value.to_string());
    }
    Ok(parameters)
}

fn parse_algorithm_parameters_for_strategy(
    strategy: &Path,
    raw: &[String],
) -> Result<HashMap<String, String>> {
    let mut parameters = project_config_parameters(strategy)?;
    parameters.extend(parse_algorithm_parameters(raw)?);
    Ok(parameters)
}

fn project_config_parameters(strategy: &Path) -> Result<HashMap<String, String>> {
    let project_dir = strategy.parent().unwrap_or_else(|| Path::new("."));
    let path = project_dir.join("config.json");
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let config = config::ProjectConfig::load(project_dir)?;
    config
        .parameters
        .into_iter()
        .map(|(key, value)| Ok((key, project_parameter_value_to_string(value)?)))
        .collect()
}

fn project_parameter_value_to_string(value: serde_json::Value) -> Result<String> {
    match value {
        serde_json::Value::String(value) => Ok(value),
        serde_json::Value::Number(value) => Ok(value.to_string()),
        serde_json::Value::Bool(value) => Ok(value.to_string()),
        serde_json::Value::Null => bail!("project config parameter values cannot be null"),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            bail!("project config parameter values must be strings, numbers, or booleans")
        }
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

    // Apply configured data-folder when --data was not explicitly provided.
    // Workspace rlean.json takes precedence over ~/.rlean/config, and relative
    // workspace paths are resolved from the directory containing rlean.json.
    if args.data == std::path::Path::new("data") {
        if let Some(folder) = config::configured_data_folder(&args.strategy)? {
            args.data = folder;
        }
    }
    tracing::info!("Data folder: {}", args.data.display());

    let (historical_provider, history_provider) = build_providers(&args)?;

    let ext = args
        .strategy
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

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
    use lean_python::report::{
        write_data_request_files, write_log_txt, write_order_events_json, write_orders_json,
        write_report, write_results_json, write_summary_json,
    };
    use lean_python::runner::{run_strategy, RunConfig};
    use lean_python::AlgorithmImports;

    ensure_python_baseline_packages()?;

    // Register the AlgorithmImports PyO3 module before starting Python.
    // Must be called before Python::initialize.
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::Python::initialize();

    let parse_date = |s: &str| -> Result<chrono::NaiveDate> {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map_err(|_| anyhow::anyhow!("invalid date '{}', expected YYYY-MM-DD", s))
    };
    let start_date_override = args.start_date.as_deref().map(parse_date).transpose()?;
    let end_date_override = args.end_date.as_deref().map(parse_date).transpose()?;
    let parameters = parse_algorithm_parameters_for_strategy(&args.strategy, &args.parameters)?;

    // Each backtest creates a LEAN-compatible output directory:
    //   <project>/backtests/YYYY-MM-DD_<strategy-name>/
    //
    // Matches C# LEAN format (e.g. "backtests/2026-04-01_sma_crossover/").
    // When --report is set it is treated as the folder path directly.
    let backtest_dir: PathBuf = if let Some(p) = args.report.clone() {
        std::fs::create_dir_all(&p)?;
        p
    } else {
        let backtests_root = args
            .strategy
            .parent() // <project>/
            .map(|p| p.join("backtests"))
            .unwrap_or_else(|| PathBuf::from("backtests"));
        let now = chrono::Utc::now();
        let name = strategy_name_from_path(&args.strategy);
        reserve_backtest_dir(&backtests_root, now, &name)?
    };

    // Snapshot the strategy source file into the backtest directory so there is
    // a permanent record of the exact code that produced this backtest.
    let code_dir = backtest_dir.join("code");
    if let Err(e) = std::fs::create_dir_all(&code_dir) {
        eprintln!("Warning: could not create code snapshot dir: {e}");
    } else {
        let dest = code_dir.join(
            args.strategy
                .file_name()
                .unwrap_or_else(|| std::ffi::OsStr::new("main.py")),
        );
        if let Err(e) = std::fs::copy(&args.strategy, &dest) {
            eprintln!("Warning: could not snapshot strategy code: {e}");
        }
    }

    // Auto-load custom data source plugins from ~/.rlean/plugins/.
    let custom_data_sources = crate::providers::load_custom_data_plugins(&args.data);

    let config = RunConfig {
        data_root: args.data.clone(),
        _compression_level: 3,
        historical_provider,
        history_provider,
        start_date_override,
        end_date_override,
        parameters,
        custom_data_sources,
        output_dir: Some(backtest_dir.clone()),
    };

    let results = run_strategy(&args.strategy, config).await?;

    results.print_summary();

    // The backtest ID (Unix epoch seconds at backtest start) is used as the
    // filename prefix for all per-backtest files, matching C# LEAN's convention.
    let id = results.backtest_id;
    // Millisecond timestamp suffix for data-request files.
    let ts_ms = chrono::Utc::now().format("%Y%m%d%H%M%S%3f");

    // ── write all output files ────────────────────────────────────────────────
    let json_path = backtest_dir.join(format!("{id}.json"));
    let order_events_path = backtest_dir.join(format!("{id}-order-events.json"));
    let orders_path = backtest_dir.join(format!("{id}-orders.json"));
    let summary_path = backtest_dir.join(format!("{id}-summary.json"));
    let id_log_path = backtest_dir.join(format!("{id}-log.txt"));
    let top_log_path = backtest_dir.join("log.txt");
    let succeeded_path = backtest_dir.join(format!("succeeded-data-requests-{ts_ms}.txt"));
    let failed_path = backtest_dir.join(format!("failed-data-requests-{ts_ms}.txt"));
    let report_path = backtest_dir.join("report.html");

    if let Err(e) = write_results_json(&results, &json_path) {
        eprintln!("Failed to write results: {e}");
    }
    if let Err(e) = write_order_events_json(&results, &order_events_path) {
        eprintln!("Failed to write order events: {e}");
    }
    if let Err(e) = write_orders_json(&results, &orders_path) {
        eprintln!("Failed to write orders: {e}");
    }
    if let Err(e) = write_summary_json(&results, &summary_path) {
        eprintln!("Failed to write summary: {e}");
    }
    if let Err(e) = write_log_txt(&results, &id_log_path) {
        eprintln!("Failed to write log: {e}");
    }
    let _ = std::fs::copy(&id_log_path, &top_log_path);
    if let Err(e) = write_data_request_files(&results, &succeeded_path, &failed_path) {
        eprintln!("Failed to write data requests: {e}");
    }
    if let Err(e) = write_report(&results, &report_path) {
        eprintln!("Failed to write report: {e}");
    }

    println!("Results: {}", backtest_dir.display());
    Ok(())
}

fn run_rust_plugin_backtest(args: RunArgs) -> Result<()> {
    use lean_algorithm::algorithm::IAlgorithm;
    use lean_engine::{BacktestEngine, EngineConfig};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LiveDeploymentMetadata {
    deploy_id: String,
    strategy: PathBuf,
    strategy_name: String,
    deployment_dir: PathBuf,
    pid: Option<u32>,
    status: String,
    launched: String,
    stopped: Option<String>,
    updated_at: String,
    brokerage: Option<String>,
    data_provider_live: Option<String>,
    data_provider_historical: Option<String>,
    data: PathBuf,
    paper_trading: bool,
    command: Vec<String>,
    error: Option<String>,
    exit_code: Option<i32>,
}

async fn run_live(args: LiveArgs) -> Result<()> {
    if let Some(command) = args.command.clone() {
        return run_live_control(command);
    }

    if args.foreground {
        if let Some(dir) = args.live_deploy_dir.as_ref() {
            update_live_deployment_status(dir, "running", None, None, Some(std::process::id()));
        }
        let deploy_dir = args.live_deploy_dir.clone();
        let result = run_live_foreground(args).await;
        if let Some(dir) = deploy_dir.as_ref() {
            match &result {
                Ok(()) => update_live_deployment_status(dir, "stopped", None, Some(0), None),
                Err(error) => update_live_deployment_status(
                    dir,
                    "runtime-error",
                    Some(error.to_string()),
                    Some(1),
                    None,
                ),
            }
        }
        return result;
    }

    launch_live_detached(args)
}

async fn run_live_foreground(args: LiveArgs) -> Result<()> {
    let deploy_dir = args.live_deploy_dir.clone();
    let mut args = args.to_run_args()?;
    args.strategy = resolve_strategy_file(args.strategy)?;
    if let Ok(canonical) = std::fs::canonicalize(&args.strategy) {
        args.strategy = canonical;
    }
    if args.strategy.is_dir() {
        args.strategy = resolve_strategy_file(args.strategy)?;
    }

    validate_strategy_path(&args.strategy)?;

    if args.data == std::path::Path::new("data") {
        if let Some(folder) = config::configured_data_folder(&args.strategy)? {
            args.data = folder;
        }
    }
    tracing::info!("Data folder: {}", args.data.display());

    let live_provider_names = args
        .data_provider_live
        .as_deref()
        .or(args.data_provider_historical.as_deref())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live trading requires --data-provider-live, for example --data-provider-live tradier"
            )
        })?;
    let requested_brokerage = args
        .brokerage
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live mode requires --brokerage paper or a live brokerage plugin, for example --brokerage tradier"
            )
        })?;
    let requested_paper_brokerage = is_paper_brokerage_name(requested_brokerage);

    ensure_python_baseline_packages()?;
    use lean_python::AlgorithmImports;
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::Python::initialize();

    let provider_args = providers::ProviderArgs {
        data_root: args.data.clone(),
        polygon_rate: args.polygon_rate,
        thetadata_rate: args.thetadata_rate,
        thetadata_concurrent: args.thetadata_concurrent,
    };
    let live_data_queue =
        providers::build_live_data_queue(live_provider_names, provider_args.clone())?;
    let brokerage = if requested_paper_brokerage {
        None
    } else {
        Some(providers::load_brokerage_plugin(
            requested_brokerage,
            &provider_args,
        )?)
    };
    let paper_trading = requested_paper_brokerage
        || brokerage
            .as_ref()
            .map(|brokerage| brokerage.uses_local_paper_fills())
            .unwrap_or(false);
    let brokerage_name = if requested_paper_brokerage {
        Some("Paper".to_string())
    } else {
        brokerage
            .as_ref()
            .map(|brokerage| brokerage.name().to_string())
    };
    let brokerage_model = live_brokerage_model_for_name(requested_brokerage);

    let (_, history_provider) = build_providers(&args)?;
    let parameters = parse_algorithm_parameters_for_strategy(&args.strategy, &args.parameters)?;
    let custom_data_sources = crate::providers::load_custom_data_plugins(&args.data);

    let ext = args
        .strategy
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if ext != "py" {
        bail!("Live trading currently supports Python strategies only");
    }

    let result = lean_python::runner::run_live_strategy(
        &args.strategy,
        lean_python::runner::LiveRunConfig {
            data_root: args.data.clone(),
            history_provider,
            parameters,
            custom_data_sources,
            live_data_queue,
            brokerage,
            brokerage_model,
            paper_trading,
            max_slices: args.live_max_slices,
            max_runtime: args.live_max_runtime_seconds.map(Duration::from_secs),
            output_dir: deploy_dir,
        },
    )
    .await?;

    if let Some(brokerage_name) = brokerage_name {
        let mode = if paper_trading {
            "paper-fill"
        } else {
            "live-order"
        };
        println!(
            "Live brokerage {} run stopped: brokerage={} slices={} final_value=${:.2} order_events={} started_at={} stopped_at={}",
            mode,
            brokerage_name,
            result.slices_processed,
            result.final_value,
            result.order_events.len(),
            result.started_at.to_rfc3339(),
            result.stopped_at.to_rfc3339()
        );
    } else {
        println!(
            "Live paper run stopped: slices={} final_value=${:.2} order_events={} started_at={} stopped_at={}",
            result.slices_processed,
            result.final_value,
            result.order_events.len(),
            result.started_at.to_rfc3339(),
            result.stopped_at.to_rfc3339()
        );
    }
    Ok(())
}

fn live_brokerage_model_for_name(name: &str) -> Option<BrokerageName> {
    let normalized = normalized_brokerage_name(name);
    match normalized.as_str() {
        "tradier" | "tradierbrokerage" => Some(BrokerageName::TradierBrokerage),
        "hyperliquid" | "hyperliquidbrokerage" => Some(BrokerageName::HyperliquidBrokerage),
        "interactivebrokers" | "interactivebrokersbrokerage" | "ib" | "ibkr" => {
            Some(BrokerageName::InteractiveBrokersBrokerage)
        }
        "quantconnect" | "quantconnectbrokerage" => Some(BrokerageName::QuantConnectBrokerage),
        "default" | "paper" | "paperbrokerage" => Some(BrokerageName::Default),
        _ => None,
    }
}

fn is_paper_brokerage_name(name: &str) -> bool {
    matches!(
        normalized_brokerage_name(name).as_str(),
        "paper" | "paperbrokerage"
    )
}

fn normalized_brokerage_name(name: &str) -> String {
    name.trim()
        .to_ascii_lowercase()
        .replace(['-', '_', ' '], "")
}

fn launch_live_detached(args: LiveArgs) -> Result<()> {
    let mut run_args = args.to_run_args()?;
    run_args.strategy = resolve_strategy_file(run_args.strategy)?;
    if let Ok(canonical) = std::fs::canonicalize(&run_args.strategy) {
        run_args.strategy = canonical;
    }
    validate_strategy_path(&run_args.strategy)?;

    if run_args.data == std::path::Path::new("data") {
        if let Some(folder) = config::configured_data_folder(&run_args.strategy)? {
            run_args.data = folder;
        }
    }
    if let Ok(canonical) = std::fs::canonicalize(&run_args.data) {
        run_args.data = canonical;
    }
    let requested_brokerage = run_args
        .brokerage
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "live mode requires --brokerage paper or a live brokerage plugin, for example --brokerage tradier"
            )
        })?;
    let paper_trading = is_paper_brokerage_name(requested_brokerage);

    let strategy_name = strategy_name_from_path(&run_args.strategy);
    let live_root = run_args
        .strategy
        .parent()
        .map(|p| p.join("live"))
        .unwrap_or_else(|| PathBuf::from("live"));
    let deployment_dir = reserve_live_dir(&live_root, chrono::Utc::now(), &strategy_name)?;
    let source_strategy = run_args.strategy.clone();
    let deployment_strategy = snapshot_strategy_code(&source_strategy, &deployment_dir)?;
    let mut child_run_args = run_args.clone();
    child_run_args.strategy = deployment_strategy.clone();

    let deploy_id = deployment_dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("deployment")
        .to_string();
    let exe = std::env::current_exe().context("failed to resolve current rlean executable")?;
    let child_args = live_child_args(&child_run_args, &deployment_dir);
    let command = std::iter::once(exe.to_string_lossy().to_string())
        .chain(child_args.iter().cloned())
        .collect::<Vec<_>>();

    let now = chrono::Utc::now().to_rfc3339();
    let initial_metadata = LiveDeploymentMetadata {
        deploy_id: deploy_id.clone(),
        strategy: source_strategy,
        strategy_name,
        deployment_dir: deployment_dir.clone(),
        pid: None,
        status: "launching".to_string(),
        launched: now.clone(),
        stopped: None,
        updated_at: now,
        brokerage: run_args.brokerage.clone(),
        data_provider_live: run_args.data_provider_live.clone(),
        data_provider_historical: run_args.data_provider_historical.clone(),
        data: run_args.data.clone(),
        paper_trading,
        command,
        error: None,
        exit_code: None,
    };
    write_live_deployment_metadata(&deployment_dir, &initial_metadata)?;

    let log_path = deployment_dir.join("live.log");
    let log_file = File::create(&log_path)
        .with_context(|| format!("failed to create {}", log_path.display()))?;
    let stderr_file = log_file
        .try_clone()
        .with_context(|| format!("failed to clone {}", log_path.display()))?;

    let mut command = ProcessCommand::new(&exe);
    command
        .args(&child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(strategy_dir) = deployment_strategy.parent() {
        command.current_dir(strategy_dir);
    }
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("failed to launch detached live deployment")?;

    write_pid_file(&deployment_dir, child.id())?;
    if let Ok(mut metadata) = read_live_deployment_metadata(&deployment_dir) {
        if !is_terminal_live_status(&metadata.status) {
            metadata.pid = Some(child.id());
            metadata.status = "running".to_string();
            metadata.updated_at = chrono::Utc::now().to_rfc3339();
            write_live_deployment_metadata(&deployment_dir, &metadata)?;
        }
    }
    register_live_deployment(&deployment_dir);

    println!(
        "Live deployment started: deploy_id={} pid={} dir={} log={}",
        deploy_id,
        child.id(),
        deployment_dir.display(),
        log_path.display()
    );
    Ok(())
}

fn run_live_control(command: LiveSubcommand) -> Result<()> {
    match command {
        LiveSubcommand::List { status } => list_live_deployments(status),
        LiveSubcommand::Status { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            let metadata =
                normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
            let effective_status = effective_live_status(&metadata);
            let running_pid = running_live_pid(&metadata);
            let process_alive = running_pid.is_some();
            let payload = serde_json::json!({
                "deploy_id": metadata.deploy_id,
                "status": effective_status,
                "recorded_status": metadata.status,
                "process_alive": process_alive,
                "pid": running_pid,
                "strategy": metadata.strategy,
                "strategy_name": metadata.strategy_name,
                "deployment_dir": metadata.deployment_dir,
                "launched": metadata.launched,
                "stopped": metadata.stopped,
                "updated_at": metadata.updated_at,
                "brokerage": metadata.brokerage,
                "data_provider_live": metadata.data_provider_live,
                "data_provider_historical": metadata.data_provider_historical,
                "data": metadata.data,
                "paper_trading": metadata.paper_trading,
                "error": metadata.error,
                "exit_code": metadata.exit_code,
                "files": {
                    "pid": dir.join("pid"),
                    "log": dir.join("live.log"),
                    "portfolio": dir.join("portfolio.json"),
                    "orders": dir.join("orders.json"),
                    "order_events": dir.join("order-events.jsonl"),
                    "trades": dir.join("trades.jsonl"),
                    "progress": dir.join("progress.json"),
                    "deployment": dir.join("deployment.json")
                }
            });
            print_json_value(&payload)
        }
        LiveSubcommand::Portfolio { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            print_json_file(&dir.join("portfolio.json"))
        }
        LiveSubcommand::Orders { deploy_id } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            print_json_file(&dir.join("orders.json"))
        }
        LiveSubcommand::Logs { deploy_id, lines } => {
            let dir = find_live_deployment_dir(&deploy_id)?;
            print_tail(&dir.join("live.log"), lines)
        }
        LiveSubcommand::Pause {
            deploy_id,
            timeout_seconds,
        } => pause_live_deployment(&deploy_id, Duration::from_secs(timeout_seconds)),
        LiveSubcommand::Resume { deploy_id } => resume_live_deployment(&deploy_id),
        LiveSubcommand::Upgrade { deploy_id } => upgrade_live_deployment(&deploy_id),
        LiveSubcommand::Remove { deploy_id, force } => remove_live_deployment(&deploy_id, force),
    }
}

fn pause_live_deployment(deploy_id: &str, timeout: Duration) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let mut metadata =
        normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status == "paused" {
        println!(
            "Live deployment already paused: deploy_id={}",
            metadata.deploy_id
        );
        return Ok(());
    }
    if effective_status != "running" {
        bail!("live deployment {deploy_id} is not running; current status is {effective_status}");
    }

    let pid = metadata
        .pid
        .ok_or_else(|| anyhow::anyhow!("live deployment {deploy_id} has no recorded pid"))?;
    terminate_process(pid)?;
    if !wait_for_process_exit(pid, timeout) {
        bail!(
            "timed out waiting for live deployment {deploy_id} pid {pid} to exit after {}s",
            timeout.as_secs()
        );
    }

    metadata.pid = None;
    metadata.status = "paused".to_string();
    metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;
    let _ = std::fs::remove_file(dir.join("pid"));

    println!(
        "Live deployment paused: deploy_id={} pid={pid}",
        metadata.deploy_id
    );
    Ok(())
}

fn resume_live_deployment(deploy_id: &str) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let mut metadata =
        normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status == "running" {
        bail!("live deployment {deploy_id} is already running");
    }
    if metadata.command.is_empty() {
        bail!("live deployment {deploy_id} has no stored command to resume");
    }

    let deployment_strategy = deployment_strategy_path(&metadata, &dir);
    if deployment_strategy.exists() {
        rewrite_live_command_strategy(&mut metadata.command, &deployment_strategy);
    }

    let exe = PathBuf::from(&metadata.command[0]);
    let child_args = metadata.command[1..].to_vec();
    let log_path = dir.join("live.log");
    let log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let stderr_file = log_file
        .try_clone()
        .with_context(|| format!("failed to clone {}", log_path.display()))?;

    let mut command = ProcessCommand::new(&exe);
    command
        .args(&child_args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr_file));
    if let Some(strategy_dir) = deployment_strategy
        .parent()
        .or_else(|| metadata.strategy.parent())
    {
        command.current_dir(strategy_dir);
    }
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }

    let child = command
        .spawn()
        .context("failed to resume live deployment")?;
    write_pid_file(&dir, child.id())?;
    metadata.pid = Some(child.id());
    metadata.status = "running".to_string();
    metadata.stopped = None;
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;
    register_live_deployment(&dir);

    println!(
        "Live deployment resumed: deploy_id={} pid={} dir={} log={}",
        metadata.deploy_id,
        child.id(),
        dir.display(),
        log_path.display()
    );
    Ok(())
}

fn upgrade_live_deployment(deploy_id: &str) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let mut metadata =
        normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status != "paused" {
        bail!(
            "live deployment {deploy_id} must be paused before upgrade; current status is {effective_status}"
        );
    }

    let deployment_strategy = snapshot_strategy_code(&metadata.strategy, &dir)?;
    rewrite_live_command_strategy(&mut metadata.command, &deployment_strategy);
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    metadata.error = None;
    metadata.exit_code = None;
    write_live_deployment_metadata(&dir, &metadata)?;

    println!(
        "Live deployment upgraded: deploy_id={} code={}",
        metadata.deploy_id,
        deployment_strategy.display()
    );
    Ok(())
}

fn remove_live_deployment(deploy_id: &str, force: bool) -> Result<()> {
    let dir = find_live_deployment_dir(deploy_id)?;
    let metadata = normalize_live_deployment_metadata(&dir, read_live_deployment_metadata(&dir)?);
    let effective_status = effective_live_status(&metadata);
    if effective_status == "running" {
        if !force {
            bail!("live deployment {deploy_id} is running; pause it first or pass --force");
        }
        if let Some(pid) = metadata.pid {
            terminate_process(pid)?;
            if !wait_for_process_exit(pid, Duration::from_secs(30)) {
                bail!("timed out waiting for live deployment {deploy_id} pid {pid} to exit");
            }
        }
    }

    unregister_live_deployment(&dir);
    std::fs::remove_dir_all(&dir).with_context(|| {
        format!(
            "failed to remove live deployment directory {}",
            dir.display()
        )
    })?;
    println!(
        "Live deployment removed: deploy_id={} dir={}",
        metadata.deploy_id,
        dir.display()
    );
    Ok(())
}

fn list_live_deployments(status_filter: Option<LiveStatusFilter>) -> Result<()> {
    let mut rows = discover_live_deployments()
        .into_iter()
        .filter_map(|dir| {
            read_live_deployment_metadata(&dir).ok().map(|metadata| {
                let normalized = normalize_live_deployment_metadata(&dir, metadata);
                (dir, normalized)
            })
        })
        .collect::<Vec<_>>();
    rows.sort_by(|(_, a), (_, b)| b.launched.cmp(&a.launched));

    let filter = status_filter.map(|status| status.as_str().to_string());
    println!(
        "{:<36} {:<14} {:<8} {:<24} {:<16} STRATEGY",
        "DEPLOY ID", "STATUS", "PID", "LAUNCHED", "BROKERAGE"
    );
    for (_, metadata) in rows {
        let status = effective_live_status(&metadata);
        if filter.as_ref().is_some_and(|wanted| wanted != &status) {
            continue;
        }
        println!(
            "{:<36} {:<14} {:<8} {:<24} {:<16} {}",
            metadata.deploy_id,
            status,
            running_live_pid(&metadata)
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
            metadata.launched,
            metadata.brokerage.as_deref().unwrap_or("Paper"),
            metadata.strategy.display()
        );
    }
    Ok(())
}

impl LiveStatusFilter {
    fn as_str(self) -> &'static str {
        match self {
            LiveStatusFilter::Running => "running",
            LiveStatusFilter::Paused => "paused",
            LiveStatusFilter::Stopped => "stopped",
            LiveStatusFilter::RuntimeError => "runtime-error",
            LiveStatusFilter::Liquidated => "liquidated",
        }
    }
}

fn live_child_args(args: &RunArgs, deployment_dir: &Path) -> Vec<String> {
    let mut values = vec![
        "live".to_string(),
        "--foreground".to_string(),
        "--live-deploy-dir".to_string(),
        deployment_dir.to_string_lossy().to_string(),
        args.strategy.to_string_lossy().to_string(),
        "--data".to_string(),
        args.data.to_string_lossy().to_string(),
    ];

    if let Some(value) = &args.data_provider_historical {
        values.extend(["--data-provider-historical".to_string(), value.clone()]);
    }
    if let Some(value) = &args.data_provider_live {
        values.extend(["--data-provider-live".to_string(), value.clone()]);
    }
    if let Some(value) = &args.brokerage {
        values.extend(["--brokerage".to_string(), value.clone()]);
    }
    if let Some(value) = &args.start_date {
        values.extend(["--start-date".to_string(), value.clone()]);
    }
    if let Some(value) = &args.end_date {
        values.extend(["--end-date".to_string(), value.clone()]);
    }
    for parameter in &args.parameters {
        values.extend(["--parameter".to_string(), parameter.clone()]);
    }
    values.extend(["--polygon-rate".to_string(), args.polygon_rate.to_string()]);
    values.extend([
        "--thetadata-rate".to_string(),
        args.thetadata_rate.to_string(),
    ]);
    values.extend([
        "--thetadata-concurrent".to_string(),
        args.thetadata_concurrent.to_string(),
    ]);
    if let Some(value) = args.live_max_slices {
        values.extend(["--live-max-slices".to_string(), value.to_string()]);
    }
    if let Some(value) = args.live_max_runtime_seconds {
        values.extend(["--live-max-runtime-seconds".to_string(), value.to_string()]);
    }
    if args.verbose {
        values.push("--verbose".to_string());
    }
    values
}

fn resolve_strategy_file(path: PathBuf) -> Result<PathBuf> {
    if path.is_dir() {
        let candidate = path.join("main.py");
        if candidate.exists() {
            return Ok(candidate);
        }
        bail!(
            "'{}' is a directory but contains no main.py. \
             Pass the strategy file directly or run `rlean create-project` to scaffold one.",
            path.display()
        );
    }
    Ok(path)
}

fn live_dir_name(datetime: chrono::DateTime<chrono::Utc>, strategy_name: &str) -> String {
    backtest_dir_name(datetime, strategy_name)
}

fn reserve_live_dir(
    live_root: &Path,
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(live_root)?;
    let base = live_dir_name(datetime, strategy_name);
    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}_{}", attempt + 1)
        };
        let candidate = live_root.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!(
        "could not reserve unique live directory under {} for {}",
        live_root.display(),
        base
    )
}

fn snapshot_strategy_code(strategy: &Path, deployment_dir: &Path) -> Result<PathBuf> {
    let code_dir = deployment_dir.join("code");
    std::fs::create_dir_all(&code_dir)
        .with_context(|| format!("failed to create {}", code_dir.display()))?;
    let dest = code_dir.join(
        strategy
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("main.py")),
    );
    std::fs::copy(strategy, &dest).with_context(|| {
        format!(
            "failed to copy {} to {}",
            strategy.display(),
            dest.display()
        )
    })?;
    Ok(dest)
}

fn deployment_strategy_path(metadata: &LiveDeploymentMetadata, dir: &Path) -> PathBuf {
    dir.join("code").join(
        metadata
            .strategy
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("main.py")),
    )
}

fn rewrite_live_command_strategy(command: &mut [String], deployment_strategy: &Path) {
    let Some(live_index) = command.iter().position(|arg| arg == "live") else {
        return;
    };
    let mut index = live_index + 1;
    while index < command.len() {
        let arg = command[index].as_str();
        if arg == "--foreground"
            || arg == "--verbose"
            || arg == "-v"
            || arg == "--help"
            || arg == "-h"
        {
            index += 1;
            continue;
        }
        if arg.starts_with('-') {
            index += 2;
            continue;
        }
        command[index] = deployment_strategy.to_string_lossy().to_string();
        return;
    }
}

fn deployment_metadata_path(dir: &Path) -> PathBuf {
    dir.join("deployment.json")
}

fn write_live_deployment_metadata(dir: &Path, metadata: &LiveDeploymentMetadata) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = deployment_metadata_path(dir);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(metadata)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn read_live_deployment_metadata(dir: &Path) -> Result<LiveDeploymentMetadata> {
    let path = deployment_metadata_path(dir);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_pid_file(dir: &Path, pid: u32) -> Result<()> {
    std::fs::write(dir.join("pid"), format!("{pid}\n"))?;
    Ok(())
}

fn update_live_deployment_status(
    dir: &Path,
    status: &str,
    error: Option<String>,
    exit_code: Option<i32>,
    pid: Option<u32>,
) {
    let Ok(mut metadata) = read_live_deployment_metadata(dir) else {
        return;
    };
    metadata.status = status.to_string();
    metadata.updated_at = chrono::Utc::now().to_rfc3339();
    if let Some(error) = error {
        metadata.error = Some(error);
    }
    if let Some(exit_code) = exit_code {
        metadata.exit_code = Some(exit_code);
    }
    if let Some(pid) = pid {
        metadata.pid = Some(pid);
        let _ = write_pid_file(dir, pid);
    }
    if is_terminal_live_status(status) {
        metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
    }
    let _ = write_live_deployment_metadata(dir, &metadata);
}

fn is_terminal_live_status(status: &str) -> bool {
    matches!(status, "stopped" | "runtime-error" | "liquidated")
}

fn effective_live_status(metadata: &LiveDeploymentMetadata) -> String {
    if matches!(metadata.status.as_str(), "running" | "launching") {
        if running_live_pid(metadata).is_some() {
            return "running".to_string();
        }
        return "stopped".to_string();
    }
    metadata.status.clone()
}

fn normalize_live_deployment_metadata(
    dir: &Path,
    mut metadata: LiveDeploymentMetadata,
) -> LiveDeploymentMetadata {
    let effective_status = effective_live_status(&metadata);
    let should_clear_pid = effective_status != "running" && metadata.pid.is_some();
    let should_update_status = metadata.status != effective_status;
    if should_clear_pid || should_update_status {
        metadata.status = effective_status;
        if metadata.status != "running" {
            metadata.pid = None;
            let _ = std::fs::remove_file(dir.join("pid"));
            if metadata.stopped.is_none() {
                metadata.stopped = Some(chrono::Utc::now().to_rfc3339());
            }
        }
        metadata.updated_at = chrono::Utc::now().to_rfc3339();
        let _ = write_live_deployment_metadata(dir, &metadata);
    }
    metadata
}

fn running_live_pid(metadata: &LiveDeploymentMetadata) -> Option<u32> {
    if !matches!(metadata.status.as_str(), "running" | "launching") {
        return None;
    }
    let pid = metadata.pid?;
    if process_matches_live_deployment(pid, &metadata.deployment_dir) {
        Some(pid)
    } else {
        None
    }
}

fn process_matches_live_deployment(pid: u32, deployment_dir: &Path) -> bool {
    if !process_is_alive(pid) {
        return false;
    }
    let Ok(output) = ProcessCommand::new("ps")
        .args(["-ww", "-p", &pid.to_string(), "-o", "command="])
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    let deployment_dir = deployment_dir.to_string_lossy();
    command.contains("--live-deploy-dir") && command.contains(deployment_dir.as_ref())
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        let error = std::io::Error::last_os_error();
        error.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    ProcessCommand::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn terminate_process(pid: u32) -> Result<()> {
    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if result == 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        Err(error).with_context(|| format!("failed to terminate pid {pid}"))
    }
    #[cfg(not(unix))]
    {
        let status = ProcessCommand::new("kill")
            .arg(pid.to_string())
            .status()
            .with_context(|| format!("failed to invoke kill for pid {pid}"))?;
        if status.success() {
            return Ok(());
        }
        bail!("failed to terminate pid {pid}: kill exited with {status}");
    }
}

fn wait_for_process_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !process_is_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn register_live_deployment(dir: &Path) {
    let Some(path) = live_registry_path() else {
        return;
    };
    let _ = std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")));
    let mut dirs = read_live_registry();
    dirs.insert(dir.to_path_buf());
    if let Ok(json) = serde_json::to_string_pretty(
        &dirs
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
    ) {
        let _ = std::fs::write(path, json);
    }
}

fn unregister_live_deployment(dir: &Path) {
    let Some(path) = live_registry_path() else {
        return;
    };
    let mut dirs = read_live_registry();
    dirs.remove(dir);
    if let Ok(canonical) = std::fs::canonicalize(dir) {
        dirs.remove(&canonical);
    }
    if let Ok(json) = serde_json::to_string_pretty(
        &dirs
            .into_iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect::<Vec<_>>(),
    ) {
        let _ = std::fs::write(path, json);
    }
}

fn read_live_registry() -> BTreeSet<PathBuf> {
    let Some(path) = live_registry_path() else {
        return BTreeSet::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn live_registry_path() -> Option<PathBuf> {
    let home = std::env::var_os("RLEAN_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".rlean")))?;
    Some(home.join("live").join("registry.json"))
}

fn discover_live_deployments() -> Vec<PathBuf> {
    let mut dirs = read_live_registry();
    if let Ok(current_dir) = std::env::current_dir() {
        collect_live_deployments_under(&current_dir, &mut dirs);
        if let Ok(entries) = std::fs::read_dir(&current_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_live_deployments_under(&path, &mut dirs);
                }
            }
        }
    }
    dirs.into_iter()
        .filter(|dir| deployment_metadata_path(dir).exists())
        .collect()
}

fn collect_live_deployments_under(root: &Path, dirs: &mut BTreeSet<PathBuf>) {
    let live_root = root.join("live");
    let Ok(entries) = std::fs::read_dir(live_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if deployment_metadata_path(&path).exists() {
            dirs.insert(path);
        }
    }
}

fn find_live_deployment_dir(deploy_id: &str) -> Result<PathBuf> {
    let path = PathBuf::from(deploy_id);
    if deployment_metadata_path(&path).exists() {
        return Ok(path);
    }
    for dir in discover_live_deployments() {
        if dir.file_name().and_then(|name| name.to_str()) == Some(deploy_id) {
            return Ok(dir);
        }
        if let Ok(metadata) = read_live_deployment_metadata(&dir) {
            if metadata.deploy_id == deploy_id {
                return Ok(dir);
            }
        }
    }
    bail!("live deployment not found: {deploy_id}")
}

fn print_json_file(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => print_json_value(&value),
        Err(_) => {
            print!("{text}");
            Ok(())
        }
    }
}

fn print_json_value(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn print_tail(path: &Path, lines: usize) -> Result<()> {
    let mut file =
        File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if lines == 0 {
        return Ok(());
    }
    let all_lines = text.lines().collect::<Vec<_>>();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn ensure_python_baseline_packages() -> Result<()> {
    let python = std::env::var("RLEAN_PYTHON").unwrap_or_else(|_| "python3".to_string());
    let packages = ["numpy", "pandas", "scipy"];
    let site_packages = rlean_python_site_packages(&python)?;
    let import_check = packages
        .iter()
        .map(|pkg| format!("import {pkg}"))
        .collect::<Vec<_>>()
        .join("; ");

    let check = python_import_check(&python, &site_packages, &import_check);
    if matches!(check, Ok(status) if status.success()) {
        return Ok(());
    }

    eprintln!(
        "Python baseline packages missing for {python}; installing LEAN-compatible defaults into {}: {}",
        site_packages.display(),
        packages.join(", ")
    );

    std::fs::create_dir_all(&site_packages)?;
    if install_python_baseline_with_uv(&site_packages, packages).is_err() {
        install_python_baseline_with_pip(&python, packages)?;
    }

    let recheck = python_import_check(&python, &site_packages, &import_check)
        .map_err(|e| anyhow::anyhow!("failed to verify Python baseline packages: {e}"))?;
    if !recheck.success() {
        bail!(
            "Python baseline packages still cannot be imported by {python}. \
             Set RLEAN_PYTHON to the interpreter used by the embedded Python runtime."
        );
    }

    Ok(())
}

fn python_import_check(
    python: &str,
    site_packages: &Path,
    import_check: &str,
) -> std::io::Result<std::process::ExitStatus> {
    ProcessCommand::new(python)
        .env("PYTHONPATH", site_packages)
        .arg("-c")
        .arg(import_check)
        .status()
}

fn install_python_baseline_with_uv(site_packages: &Path, packages: [&str; 3]) -> Result<()> {
    let python_platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-manylinux_2_28",
        ("linux", "x86_64") => "x86_64-manylinux_2_28",
        _ => "aarch64-apple-darwin",
    };

    let status = ProcessCommand::new("uv")
        .args([
            "pip",
            "install",
            "--target",
            site_packages.to_string_lossy().as_ref(),
            "--python-version",
            "3.14",
            "--python-platform",
            python_platform,
        ])
        .args(packages)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `uv pip install`: {e}"))?;
    if !status.success() {
        bail!("`uv pip install` failed");
    }
    Ok(())
}

fn install_python_baseline_with_pip(python: &str, packages: [&str; 3]) -> Result<()> {
    let ensurepip_status = ProcessCommand::new(python)
        .args(["-m", "ensurepip", "--upgrade"])
        .status();
    if !matches!(ensurepip_status, Ok(status) if status.success()) {
        eprintln!("Warning: `{python} -m ensurepip --upgrade` did not complete successfully");
    }

    let status = ProcessCommand::new(python)
        .args(["-m", "pip", "install", "--upgrade"])
        .args(packages)
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "failed to run `{python} -m pip install --upgrade numpy pandas scipy`: {e}"
            )
        })?;
    if !status.success() {
        bail!(
            "failed to install Python baseline packages for {python}. \
             Install them manually with `{python} -m pip install --upgrade numpy pandas scipy`, \
             or set RLEAN_PYTHON to the Python interpreter rlean should use."
        );
    }
    Ok(())
}

fn rlean_python_site_packages(python: &str) -> Result<PathBuf> {
    let tag = ProcessCommand::new(python)
        .arg("-c")
        .arg("import sys; print(f'cp{sys.version_info.major}{sys.version_info.minor}')")
        .output()
        .map_err(|e| anyhow::anyhow!("failed to query Python version from {python}: {e}"))?;
    if !tag.status.success() {
        bail!("failed to query Python version from {python}");
    }
    let tag = String::from_utf8(tag.stdout)
        .context("Python version output was not UTF-8")?
        .trim()
        .to_string();

    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".rlean")
        .join("python")
        .join(tag)
        .join("site-packages"))
}

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
    let stem = strategy
        .file_stem()
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
pub(crate) fn backtest_dir_name(
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> String {
    format!("{}_{}", datetime.format("%Y-%m-%d_%H%M%S"), strategy_name)
}

fn reserve_backtest_dir(
    backtests_root: &Path,
    datetime: chrono::DateTime<chrono::Utc>,
    strategy_name: &str,
) -> Result<PathBuf> {
    std::fs::create_dir_all(backtests_root)?;
    let base = backtest_dir_name(datetime, strategy_name);
    for attempt in 0..1000 {
        let name = if attempt == 0 {
            base.clone()
        } else {
            format!("{base}_{}", attempt + 1)
        };
        let candidate = backtests_root.join(name);
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!(
        "could not reserve unique backtest directory under {} for {}",
        backtests_root.display(),
        base
    )
}

fn validate_strategy_path(path: &Path) -> Result<()> {
    if !path.exists() {
        bail!("Strategy file not found: {}", path.display());
    }
    Ok(())
}

fn build_providers(args: &RunArgs) -> Result<ProviderPair> {
    let names = match args.data_provider_historical.as_deref() {
        Some(n) => n,
        None => return Ok((None, None)),
    };

    let provider_args = providers::ProviderArgs {
        data_root: args.data.clone(),
        polygon_rate: args.polygon_rate,
        thetadata_rate: args.thetadata_rate,
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
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = lean_core::Result<Vec<lean_data::TradeBar>>>
                + Send
                + '_,
        >,
    > {
        let provider = Arc::clone(&self.0);
        let request = lean_data_providers::HistoryRequest {
            symbol: symbol.clone(),
            resolution,
            start,
            end,
            data_type: lean_data_providers::DataType::TradeBar,
        };
        Box::pin(async move {
            provider
                .get_history(&request)
                .await
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))
        })
    }

    fn earliest_date(&self) -> Option<chrono::NaiveDate> {
        self.0.earliest_date()
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

    #[test]
    fn test_project_config_parameters_are_loaded_and_cli_overrides() {
        let root =
            std::env::temp_dir().join(format!("rlean-project-params-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.py"), "").unwrap();
        std::fs::write(
            root.join("config.json"),
            r#"{
  "algorithm-language": "python",
  "parameters": {
    "max_holds": "30",
    "enabled": true,
    "threshold": 2.5
  },
  "description": "",
  "local-id": 123
}"#,
        )
        .unwrap();

        let parameters = parse_algorithm_parameters_for_strategy(
            &root.join("main.py"),
            &["max_holds=12".to_string(), "extra=value".to_string()],
        )
        .unwrap();

        assert_eq!(parameters.get("max_holds").map(String::as_str), Some("12"));
        assert_eq!(parameters.get("enabled").map(String::as_str), Some("true"));
        assert_eq!(parameters.get("threshold").map(String::as_str), Some("2.5"));
        assert_eq!(parameters.get("extra").map(String::as_str), Some("value"));

        let _ = std::fs::remove_dir_all(&root);
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
        let d1 = backtest_dir_name(dt1, "spy_wheel");
        let d2 = backtest_dir_name(dt2, "spy_wheel");
        assert_ne!(d1, d2, "runs on same day must produce different dirs");
    }

    #[test]
    fn test_backtest_dir_name_date_prefix() {
        use chrono::{TimeZone, Utc};
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 9, 5, 3).unwrap();
        let dir = backtest_dir_name(dt, "sma_crossover");
        assert!(dir.starts_with("2026-04-10_090503_"), "dir={dir}");
    }

    #[test]
    fn test_reserve_backtest_dir_adds_suffix_on_collision() {
        use chrono::{TimeZone, Utc};
        let root =
            std::env::temp_dir().join(format!("rlean-backtest-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dt = Utc.with_ymd_and_hms(2026, 4, 10, 14, 30, 0).unwrap();

        let first = reserve_backtest_dir(&root, dt, "strategy").unwrap();
        let second = reserve_backtest_dir(&root, dt, "strategy").unwrap();

        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("2026-04-10_143000_strategy")
        );
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("2026-04-10_143000_strategy_2")
        );
        assert!(first.is_dir());
        assert!(second.is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_live_cli_strategy_launch_parse() {
        let cli = Cli::try_parse_from([
            "rlean",
            "live",
            "main.py",
            "--data",
            "/data",
            "--data-provider-live",
            "hyperliquid",
            "--brokerage",
            "hyperliquid",
        ])
        .unwrap();

        match cli.command {
            Command::Live(args) => {
                assert!(args.command.is_none());
                assert_eq!(args.strategy.as_deref(), Some(Path::new("main.py")));
                assert_eq!(args.data, PathBuf::from("/data"));
                assert_eq!(args.data_provider_live.as_deref(), Some("hyperliquid"));
                assert_eq!(args.brokerage.as_deref(), Some("hyperliquid"));
            }
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_live_cli_rejects_removed_live_trading_flag() {
        assert!(Cli::try_parse_from([
            "rlean",
            "live",
            "main.py",
            "--data-provider-live",
            "tradier",
            "--brokerage",
            "tradier",
            "--live-trading",
        ])
        .is_err());
    }

    #[test]
    fn test_live_cli_list_parse() {
        let cli =
            Cli::try_parse_from(["rlean", "live", "list", "--status", "runtime-error"]).unwrap();

        match cli.command {
            Command::Live(args) => match args.command {
                Some(LiveSubcommand::List { status }) => {
                    assert_eq!(status, Some(LiveStatusFilter::RuntimeError));
                    assert!(args.strategy.is_none());
                }
                _ => panic!("expected live list command"),
            },
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_live_cli_list_parse_paused_status() {
        let cli = Cli::try_parse_from(["rlean", "live", "list", "--status", "paused"]).unwrap();

        match cli.command {
            Command::Live(args) => match args.command {
                Some(LiveSubcommand::List { status }) => {
                    assert_eq!(status, Some(LiveStatusFilter::Paused));
                    assert!(args.strategy.is_none());
                }
                _ => panic!("expected live list command"),
            },
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_live_cli_pause_parse() {
        let cli = Cli::try_parse_from([
            "rlean",
            "live",
            "pause",
            "deploy-1",
            "--timeout-seconds",
            "5",
        ])
        .unwrap();

        match cli.command {
            Command::Live(args) => match args.command {
                Some(LiveSubcommand::Pause {
                    deploy_id,
                    timeout_seconds,
                }) => {
                    assert_eq!(deploy_id, "deploy-1");
                    assert_eq!(timeout_seconds, 5);
                    assert!(args.strategy.is_none());
                }
                _ => panic!("expected live pause command"),
            },
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_live_cli_resume_parse() {
        let cli = Cli::try_parse_from(["rlean", "live", "resume", "deploy-1"]).unwrap();

        match cli.command {
            Command::Live(args) => match args.command {
                Some(LiveSubcommand::Resume { deploy_id }) => {
                    assert_eq!(deploy_id, "deploy-1");
                    assert!(args.strategy.is_none());
                }
                _ => panic!("expected live resume command"),
            },
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_live_cli_upgrade_parse() {
        let cli = Cli::try_parse_from(["rlean", "live", "upgrade", "deploy-1"]).unwrap();

        match cli.command {
            Command::Live(args) => match args.command {
                Some(LiveSubcommand::Upgrade { deploy_id }) => {
                    assert_eq!(deploy_id, "deploy-1");
                    assert!(args.strategy.is_none());
                }
                _ => panic!("expected live upgrade command"),
            },
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_live_cli_remove_parse() {
        let cli = Cli::try_parse_from(["rlean", "live", "remove", "deploy-1", "--force"]).unwrap();

        match cli.command {
            Command::Live(args) => match args.command {
                Some(LiveSubcommand::Remove { deploy_id, force }) => {
                    assert_eq!(deploy_id, "deploy-1");
                    assert!(force);
                    assert!(args.strategy.is_none());
                }
                _ => panic!("expected live remove command"),
            },
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_rewrite_live_command_strategy_uses_snapshot_path() {
        let mut command = vec![
            "/usr/local/bin/rlean".to_string(),
            "live".to_string(),
            "--foreground".to_string(),
            "--live-deploy-dir".to_string(),
            "/tmp/deploy".to_string(),
            "/tmp/source/main.py".to_string(),
            "--data".to_string(),
            "/tmp/data".to_string(),
            "--verbose".to_string(),
        ];

        rewrite_live_command_strategy(&mut command, Path::new("/tmp/deploy/code/main.py"));

        assert_eq!(command[5], "/tmp/deploy/code/main.py");
        assert_eq!(command[7], "/tmp/data");
    }

    #[test]
    fn test_live_cli_foreground_child_parse() {
        let cli = Cli::try_parse_from([
            "rlean",
            "live",
            "--foreground",
            "--live-deploy-dir",
            "/tmp/deploy",
            "/tmp/strategy/main.py",
            "--data",
            "/tmp/data",
            "--data-provider-live",
            "hyperliquid",
        ])
        .unwrap();

        match cli.command {
            Command::Live(args) => {
                assert!(args.foreground);
                assert_eq!(args.live_deploy_dir, Some(PathBuf::from("/tmp/deploy")));
                assert_eq!(
                    args.strategy.as_deref(),
                    Some(Path::new("/tmp/strategy/main.py"))
                );
                assert_eq!(args.data, PathBuf::from("/tmp/data"));
                assert_eq!(args.data_provider_live.as_deref(), Some("hyperliquid"));
            }
            _ => panic!("expected live command"),
        }
    }

    #[test]
    fn test_reserve_live_dir_adds_suffix_on_collision() {
        use chrono::{TimeZone, Utc};
        let root = std::env::temp_dir().join(format!("rlean-live-dir-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dt = Utc.with_ymd_and_hms(2026, 6, 13, 14, 30, 0).unwrap();

        let first = reserve_live_dir(&root, dt, "strategy").unwrap();
        let second = reserve_live_dir(&root, dt, "strategy").unwrap();

        assert_eq!(
            first.file_name().and_then(|n| n.to_str()),
            Some("2026-06-13_143000_strategy")
        );
        assert_eq!(
            second.file_name().and_then(|n| n.to_str()),
            Some("2026-06-13_143000_strategy_2")
        );
        assert!(first.is_dir());
        assert!(second.is_dir());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_live_child_args_include_foreground_and_deploy_dir() {
        let args = RunArgs {
            strategy: PathBuf::from("/tmp/strategy/main.py"),
            data: PathBuf::from("/tmp/data"),
            data_provider_historical: Some("hyperliquid".to_string()),
            data_provider_live: Some("hyperliquid".to_string()),
            brokerage: Some("hyperliquid".to_string()),
            start_date: None,
            end_date: None,
            parameters: vec!["foo=bar".to_string()],
            polygon_rate: 5.0,
            thetadata_rate: 4.0,
            thetadata_concurrent: 4,
            report: None,
            live_max_slices: Some(2),
            live_max_runtime_seconds: Some(10),
            verbose: true,
        };
        let child_args = live_child_args(&args, Path::new("/tmp/strategy/live/deploy"));

        assert_eq!(child_args[0], "live");
        assert!(child_args.contains(&"--foreground".to_string()));
        assert!(child_args.contains(&"--live-deploy-dir".to_string()));
        assert!(!child_args.contains(&"--live-trading".to_string()));
        assert!(child_args.contains(&"/tmp/strategy/live/deploy".to_string()));
        assert!(child_args.contains(&"--parameter".to_string()));
        assert!(child_args.contains(&"foo=bar".to_string()));
        assert!(child_args.contains(&"--verbose".to_string()));
    }

    #[test]
    fn test_live_brokerage_model_name_mapping() {
        assert_eq!(
            live_brokerage_model_for_name("tradier"),
            Some(BrokerageName::TradierBrokerage)
        );
        assert_eq!(
            live_brokerage_model_for_name("Tradier-Brokerage"),
            Some(BrokerageName::TradierBrokerage)
        );
        assert_eq!(
            live_brokerage_model_for_name("hyperliquid"),
            Some(BrokerageName::HyperliquidBrokerage)
        );
        assert_eq!(
            live_brokerage_model_for_name("PaperBrokerage"),
            Some(BrokerageName::Default)
        );
        assert!(is_paper_brokerage_name("paper"));
        assert!(is_paper_brokerage_name("PaperBrokerage"));
        assert!(!is_paper_brokerage_name("tradier"));
        assert_eq!(live_brokerage_model_for_name("custom"), None);
    }
}
