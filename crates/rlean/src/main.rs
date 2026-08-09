//! rlean — unified Lean-Rust execution CLI
//!
//! Usage:
//!   rlean init                                   # bootstrap workspace
//!   rlean create-project <name>                  # scaffold a new strategy project
//!   rlean backtest <strategy> [OPTIONS]           # run a backtest
//!   rlean live     <strategy> [OPTIONS]           # run live trading
//!   rlean research <project> [OPTIONS]            # launch Jupyter research session
//!
//! Strategies are Python files using AlgorithmImports / QCAlgorithm.
//!
//! Examples:
//!   rlean init
//!   rlean create-project my_strategy
//!   rlean backtest my_strategy/main.py --thetadata-api-key $THETADATA_API_KEY
//!   rlean research my_strategy

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

mod backtest;
mod cli;
mod cloud;
mod config;
mod config_cmd;
mod container_runtime;
mod data_cmd;
mod init;
mod live;
mod live_deployments;
mod project;
mod research;
mod research_daemon;
mod run_catalog;
mod runs_cmd;
mod runtime;
mod stubs_cmd;
mod vcs_cmd;

use cli::{Cli, Command};
use config_cmd::run_config;
use data_cmd::run_data;
use init::run_init;
use project::run_create_project;
use research::run_research;
use research_daemon::run_daemon;
use stubs_cmd::run_stubs;
use vcs_cmd::run_vcs;

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    install_immediate_ctrl_c_handler();

    let verbose = match &cli.command {
        Command::Backtest(args) => args.verbose,
        Command::Live(args) => args.verbose,
        _ => false,
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if verbose {
            EnvFilter::new(
                "info,rlean=debug,rlean_algorithm=debug,rlean_core=debug,rlean_data=debug,\
                 rlean_engine=debug,lean_python=debug",
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
        Command::Data(args) => run_data(args).await,
        Command::Backtest(args) => backtest::run(args).await,
        Command::Runs(args) => runs_cmd::run(args).await,
        Command::Live(args) => live::run(args).await,
        Command::Research(args) => run_research(args),
        Command::Stubs(args) => run_stubs(args),
        Command::Vcs(args) => run_vcs(args),
        Command::Cloud(args) => cloud::run(args),
        Command::ResearchDaemon(args) => run_daemon(args),
    }
}

/// Install a low-level, async-signal-safe SIGINT/SIGTERM handler that force
/// terminates the process immediately.
///
/// This deliberately bypasses tokio, the `ctrlc` crate's background thread, and
/// `std::process::exit`. Ctrl+C must kill the process no matter what any lower
/// layer (embedded Python, allocator locks) is doing,
/// so the handler only performs async-signal-safe work: a bare `write(2)` and
/// `_exit(2)`. It runs no destructors and touches no allocator, so it cannot
/// deadlock even if the signal interrupts an allocation or a foreign call.
#[cfg(unix)]
fn install_immediate_ctrl_c_handler() {
    extern "C" fn handle(_sig: libc::c_int) {
        const MSG: &[u8] = b"\nInterrupted.\n";
        unsafe {
            libc::write(
                libc::STDERR_FILENO,
                MSG.as_ptr() as *const libc::c_void,
                MSG.len(),
            );
            libc::_exit(130);
        }
    }

    unsafe {
        // Make sure SIGINT/SIGTERM are not left blocked by any thread's mask.
        let mut unblock = std::mem::zeroed::<libc::sigset_t>();
        libc::sigemptyset(&mut unblock);
        libc::sigaddset(&mut unblock, libc::SIGINT);
        libc::sigaddset(&mut unblock, libc::SIGTERM);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &unblock, std::ptr::null_mut());

        let mut action = std::mem::zeroed::<libc::sigaction>();
        action.sa_sigaction = handle as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        // No SA_RESTART: we want blocking syscalls to abort, and the handler
        // exits anyway. Do not set SA_RESETHAND so repeated presses stay handled.
        action.sa_flags = 0;
        libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
}

#[cfg(not(unix))]
fn install_immediate_ctrl_c_handler() {
    let _ = ctrlc::set_handler(|| {
        eprintln!("\nInterrupted.");
        std::process::exit(130);
    });
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{LiveStatusFilter, LiveSubcommand};
    use std::path::{Path, PathBuf};

    #[test]
    fn test_data_init_cli_is_removed() {
        assert!(Cli::try_parse_from(["rlean", "data", "init"]).is_err());
    }

    #[test]
    fn test_data_schema_cli_parse() {
        let cli =
            Cli::try_parse_from(["rlean", "data", "schema", "rlean.market_quote_bars"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Data(crate::data_cmd::DataArgs {
                command: crate::data_cmd::DataCommand::Schema { table }
            }) if table == "rlean.market_quote_bars"
        ));
    }

    #[test]
    fn test_data_tables_cli_parse() {
        let cli = Cli::try_parse_from(["rlean", "data", "tables"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Data(crate::data_cmd::DataArgs {
                command: crate::data_cmd::DataCommand::Tables
            })
        ));
    }

    #[test]
    fn test_live_cli_strategy_launch_parse() {
        let cli = Cli::try_parse_from([
            "rlean",
            "live",
            "main.py",
            "--live-data-feed",
            "tradier",
            "--brokerage",
            "fidelity",
            "--brokerage-url",
            "http://127.0.0.1:5199",
            "--brokerage-account",
            "account-1234",
        ])
        .unwrap();

        match cli.command {
            Command::Live(args) => {
                assert!(args.command.is_none());
                assert_eq!(args.strategy.as_deref(), Some(Path::new("main.py")));
                assert_eq!(args.live_data_feed.as_deref(), Some("tradier"));
                assert_eq!(args.brokerage.as_deref(), Some("fidelity"));
                assert_eq!(args.brokerage_url.as_deref(), Some("http://127.0.0.1:5199"));
                assert_eq!(args.brokerage_account.as_deref(), Some("account-1234"));
            }
            _ => panic!("expected live command"),
        }
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
    fn test_live_cli_foreground_child_parse() {
        let cli = Cli::try_parse_from([
            "rlean",
            "live",
            "--foreground",
            "--live-deploy-dir",
            "/tmp/deploy",
            "/tmp/strategy/main.py",
            "--live-data-feed",
            "tradier",
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
                assert_eq!(args.live_data_feed.as_deref(), Some("tradier"));
            }
            _ => panic!("expected live command"),
        }
    }
}
