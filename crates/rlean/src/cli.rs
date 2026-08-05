use std::ops::{Deref, DerefMut};
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

use crate::cloud::CloudArgs;
use crate::config_cmd::ConfigArgs;
use crate::daemon_cmd::DaemonArgs;
use crate::data_cmd::DataArgs;
use crate::init::InitArgs;
use crate::project::CreateProjectArgs;
use crate::research::ResearchArgs;
use crate::research_daemon::ResearchDaemonArgs;
use crate::runs_cmd::RunsArgs;
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
    /// Bootstrap a Lean workspace in the current directory
    Init(InitArgs),

    /// Scaffold a new strategy project
    #[command(name = "create-project")]
    CreateProject(CreateProjectArgs),

    /// Get, set, or list runtime and provider integration configuration
    Config(ConfigArgs),

    /// Inspect the canonical data contract
    Data(DataArgs),

    /// Run a backtest
    Backtest(RunArgs),

    /// Inspect durable backtest results in the Verglas catalog
    Runs(RunsArgs),

    /// Run live trading or inspect local live deployments
    Live(LiveArgs),

    /// Launch an interactive research session for a project (opens research.ipynb)
    Research(ResearchArgs),

    /// Generate and install AlgorithmImports.pyi stub files for IDE autocomplete
    Stubs(StubsArgs),

    /// Version control: push, pull, sync, and configure your strategy remote
    Vcs(VcsArgs),

    /// Manage a fleet of remote nodes reachable over SSH
    Cloud(CloudArgs),

    /// Install and control the persistent live-deployment supervisor
    Daemon(DaemonArgs),

    /// Hidden: persistent PyO3 research kernel daemon (started by `rlean research`)
    #[command(name = "__research-daemon", hide = true)]
    ResearchDaemon(ResearchDaemonArgs),
}

#[derive(clap::Args, Clone)]
pub(crate) struct RuntimeArgs {
    // ── Data ─────────────────────────────────────────────────────────────────
    /// Historical market-data provider. Provider responses are normalized to
    /// rlean's canonical tables and cached through Verglas before consumption.
    #[arg(long, env = "RLEAN_DATA_PROVIDER_HISTORICAL")]
    pub(crate) data_provider_historical: Option<String>,

    /// Live market-data provider. Symbols and resolutions remain
    /// strategy SDK subscriptions; this selects the integration serving them.
    #[arg(long, env = "RLEAN_LIVE_DATA_FEED")]
    pub(crate) live_data_feed: Option<String>,

    /// Execution brokerage for live runs. Use "paper" for local simulated
    /// fills, or a native brokerage such as "tradier" for live orders. This
    /// is independent from --live-data-feed.
    #[arg(long, env = "RLEAN_BROKERAGE")]
    pub(crate) brokerage: Option<String>,

    /// Base URL implementing rlean's HTTP brokerage contract. Required when
    /// --brokerage http is selected.
    #[arg(long, env = "RLEAN_BROKERAGE_URL")]
    pub(crate) brokerage_url: Option<String>,

    /// Brokerage account identifier for this live deployment. This is passed
    /// to the selected brokerage when the connection is opened.
    #[arg(long, env = "RLEAN_BROKERAGE_ACCOUNT")]
    pub(crate) brokerage_account: Option<String>,

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
    /// Path to the Python strategy file (.py)
    pub(crate) strategy: PathBuf,

    #[command(flatten)]
    pub(crate) runtime: RuntimeArgs,

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
    Restarting,
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

impl LiveArgs {
    pub(crate) fn to_run_args(&self) -> Result<RunArgs> {
        let strategy = self
            .strategy
            .clone()
            .ok_or_else(|| anyhow::anyhow!("missing strategy path"))?;
        Ok(RunArgs {
            strategy,
            runtime: self.runtime.clone(),
            live_limits: self.live_limits.clone(),
        })
    }
}
