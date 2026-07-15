/// `rlean stubs create` — install AlgorithmImports.pyi for the PyO3 runtime module.
use std::path::PathBuf;
use std::process::Command as SysCommand;

use anyhow::{Context, Result};
use clap::Subcommand;

// ─── CLI args ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct StubsArgs {
    #[command(subcommand)]
    pub command: StubsCommand,
}

#[derive(Subcommand)]
pub enum StubsCommand {
    /// Generate AlgorithmImports.pyi and install it into the active Python
    /// environment (site-packages) and the current working directory.
    Create {
        /// Override the output directory.  When omitted the stubs are written
        /// to both `python3 site-packages` and the current working directory.
        #[arg(long)]
        output: Option<PathBuf>,
    },
}

// ─── Entry point ──────────────────────────────────────────────────────────────

pub fn run_stubs(args: StubsArgs) -> Result<()> {
    match args.command {
        StubsCommand::Create { output } => run_create(output),
    }
}

fn run_create(output_override: Option<PathBuf>) -> Result<()> {
    let cwd = std::env::current_dir().context("Cannot determine current directory")?;

    let destinations: Vec<PathBuf> = if let Some(dir) = output_override {
        vec![dir.join("AlgorithmImports.pyi")]
    } else {
        let mut dests = vec![cwd.join("AlgorithmImports.pyi")];

        // Also install into Python site-packages when python3 is available.
        match site_packages_dir() {
            Ok(site) => dests.push(site.join("AlgorithmImports.pyi")),
            Err(e) => {
                eprintln!(
                    "warning: could not determine Python site-packages directory: {e}\n\
                     Stubs will only be written to the current directory."
                );
            }
        }

        dests
    };

    let stub_content = algorithm_imports_pyi();

    for dest in &destinations {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Cannot create directory '{}'", parent.display()))?;
        }
        std::fs::write(dest, stub_content)
            .with_context(|| format!("Cannot write stubs to '{}'", dest.display()))?;
        println!("Stubs written: {}", dest.display());
    }

    println!(
        "\nAlgorithmImports.pyi installed.  \
         Add the stub location to your IDE's `python.analysis.extraPaths` \
         (Pylance) or `stubPath` (Pyright) if it is not already on the path."
    );

    Ok(())
}

fn algorithm_imports_pyi() -> &'static str {
    include_str!("../../../crates/rlean-sdk/AlgorithmImports.pyi")
}

/// Query the active Python interpreter for the primary site-packages directory.
fn site_packages_dir() -> Result<PathBuf> {
    let out = SysCommand::new("python3")
        .args(["-c", "import site; print(site.getsitepackages()[0])"])
        .output()
        .context("Failed to run python3")?;

    if !out.status.success() {
        anyhow::bail!(
            "python3 exited with status {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let path = String::from_utf8(out.stdout)
        .context("python3 output is not valid UTF-8")?
        .trim()
        .to_string();

    Ok(PathBuf::from(path))
}
