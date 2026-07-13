use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::config_cmd::ConfigArgs;
use crate::init::InitArgs;
use crate::plugin_cmd::PluginArgs;
use crate::project::CreateProjectArgs;
use crate::registry_cmd::RegistryArgs;
use crate::research::ResearchArgs;
use crate::research_daemon::ResearchDaemonArgs;
use crate::stubs_cmd::StubsArgs;
use crate::vcs_cmd::VcsArgs;

#[derive(Parser)]
#[command(
    name = "rlean",
    about = "Lean-Rust backtest, live trading, and research runner",
    version
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Bootstrap a Lean workspace in the current directory (creates rlean.json and data/)
    Init(InitArgs),

    /// Scaffold a new strategy project
    #[command(name = "create-project")]
    CreateProject(CreateProjectArgs),

    /// Get, set, or list configuration values (API keys, language, data catalog, data-folder)
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
pub(crate) struct RuntimeArgs {
    // ── Data ─────────────────────────────────────────────────────────────────
    /// Parquet data root directory
    #[arg(long, default_value = "data", env = "RLEAN_DATA")]
    pub(crate) data: PathBuf,

    /// REST Iceberg catalog base URI (e.g.
    /// https://s3tables.us-west-2.amazonaws.com/iceberg). Overrides
    /// RLEAN_DATA_CATALOG and the config file. Env is read inside resolve, so
    /// this flag is only set when explicitly passed.
    #[arg(long)]
    pub(crate) data_catalog: Option<String>,

    /// Warehouse identifier / S3 Tables table-bucket ARN. Overrides
    /// RLEAN_DATA_WAREHOUSE and the config file.
    #[arg(long)]
    pub(crate) data_warehouse: Option<String>,

    /// SigV4 signing region for the REST catalog (e.g. us-west-2). Overrides
    /// RLEAN_DATA_SIGV4_REGION and the config file.
    #[arg(long)]
    pub(crate) data_sigv4_region: Option<String>,

    /// SigV4 signing name for the REST catalog (e.g. s3tables). Overrides
    /// RLEAN_DATA_SIGV4_NAME and the config file.
    #[arg(long)]
    pub(crate) data_sigv4_name: Option<String>,

    /// Iceberg namespace holding the cache tables (default lean). Overrides
    /// RLEAN_DATA_NAMESPACE and the config file.
    #[arg(long)]
    pub(crate) data_namespace: Option<String>,

    /// Comma-separated provider priority list (e.g. thetadata,polygon)
    #[arg(long, env = "RLEAN_DATA_PROVIDER_HISTORICAL")]
    pub(crate) data_provider_historical: Option<String>,

    /// Live data provider plugin(s), comma-separated for stacked live feeds
    #[arg(long, env = "RLEAN_DATA_PROVIDER_LIVE")]
    pub(crate) data_provider_live: Option<String>,

    /// Brokerage for live runs. Use "paper" for simulated fills, or a real
    /// brokerage plugin name such as "tradier" or "hyperliquid" for live orders.
    #[arg(long, env = "RLEAN_BROKERAGE")]
    pub(crate) brokerage: Option<String>,

    // ── Date range override ───────────────────────────────────────────────────
    /// Override the strategy start date (YYYY-MM-DD)
    #[arg(long)]
    pub(crate) start_date: Option<String>,

    /// Override the strategy end date (YYYY-MM-DD)
    #[arg(long)]
    pub(crate) end_date: Option<String>,

    /// Algorithm parameter as KEY=VALUE for LEAN-style GetParameter access.
    #[arg(long = "parameter", short = 'p', value_name = "KEY=VALUE", action = clap::ArgAction::Append)]
    pub(crate) parameters: Vec<String>,

    // ── Run artifact relay ────────────────────────────────────────────────────
    /// Where run artifacts (backtest/live run dirs) are written: local, s3, or
    /// mirror. Defaults to local. Overrides RLEAN_ARTIFACT_STORE and config.
    #[arg(long, value_name = "local|s3|mirror", env = "RLEAN_ARTIFACT_STORE")]
    pub(crate) artifact_store: Option<String>,

    /// S3 destination for run artifacts as s3://bucket/prefix. Required when
    /// --artifact-store is s3 or mirror. Overrides RLEAN_ARTIFACT_S3 and config.
    #[arg(long, value_name = "s3://bucket/prefix", env = "RLEAN_ARTIFACT_S3")]
    pub(crate) artifact_s3: Option<String>,

    // ── Logging ───────────────────────────────────────────────────────────────
    /// Enable debug logging for rlean crates
    #[arg(long, short = 'v')]
    pub(crate) verbose: bool,
}

#[derive(clap::Args, Clone)]
pub(crate) struct LiveLimitArgs {
    /// Stop a live run after N slices. Useful for smoke tests.
    #[arg(long, hide = true)]
    pub(crate) live_max_slices: Option<usize>,

    /// Stop a live run after this many wall-clock seconds. Useful for paper soaks.
    #[arg(long, hide = true)]
    pub(crate) live_max_runtime_seconds: Option<u64>,
}

#[derive(clap::Args, Clone)]
pub(crate) struct RunArgs {
    /// Path to the strategy file (.py) or compiled plugin (.so/.dylib)
    pub(crate) strategy: PathBuf,

    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,

    // ── Output ────────────────────────────────────────────────────────────────
    /// Override the report output path (default: <project>/backtests/<timestamp>.html)
    #[arg(long)]
    pub(crate) report: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) live_limits: LiveLimitArgs,
}

#[derive(clap::Args, Clone)]
pub(crate) struct LiveArgs {
    #[command(subcommand)]
    pub(crate) command: Option<LiveSubcommand>,

    /// Path to the strategy file (.py) or project directory containing main.py
    pub(crate) strategy: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,

    #[command(flatten)]
    pub(crate) live_limits: LiveLimitArgs,

    /// Override the live deployment directory.
    #[arg(long, hide = true)]
    pub(crate) live_deploy_dir: Option<PathBuf>,

    /// Run live trading in the foreground instead of creating a detached deployment.
    #[arg(long, hide = true)]
    pub(crate) foreground: bool,
}

#[derive(Subcommand, Clone)]
pub(crate) enum LiveSubcommand {
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
pub(crate) enum LiveStatusFilter {
    Running,
    Paused,
    Stopped,
    RuntimeError,
    Liquidated,
}

impl Deref for RunArgs {
    type Target = RuntimeArgs;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl DerefMut for RunArgs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

impl Deref for LiveArgs {
    type Target = RuntimeArgs;

    fn deref(&self) -> &Self::Target {
        &self.runtime
    }
}

impl DerefMut for LiveArgs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.runtime
    }
}

impl RuntimeArgs {
    /// Apply the `--data-*` catalog flags so they win over the environment and
    /// the config file.
    ///
    /// Precedence is **CLI flag > env var > config file**. `data_store_config::
    /// resolve` reads the env var (falling back to the config file) for each
    /// key, so to make a CLI flag beat an inherited env var we overwrite the
    /// process env var with the flag value here. When a flag is absent the env
    /// var / config file are left to decide, preserving env > config.
    pub(crate) fn apply_data_catalog_overrides(&self) {
        let overrides = [
            ("RLEAN_DATA_CATALOG", self.data_catalog.as_deref()),
            ("RLEAN_DATA_WAREHOUSE", self.data_warehouse.as_deref()),
            ("RLEAN_DATA_SIGV4_REGION", self.data_sigv4_region.as_deref()),
            ("RLEAN_DATA_SIGV4_NAME", self.data_sigv4_name.as_deref()),
            ("RLEAN_DATA_NAMESPACE", self.data_namespace.as_deref()),
        ];
        for (var, value) in overrides {
            if let Some(value) = value {
                std::env::set_var(var, value);
            }
        }
    }
}

impl LiveArgs {
    pub(crate) fn to_run_args(&self) -> Result<RunArgs> {
        let strategy = self
            .strategy
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing strategy path"))?;
        Ok(RunArgs {
            strategy,
            runtime: self.runtime.clone(),
            report: None,
            live_limits: self.live_limits.clone(),
        })
    }
}
