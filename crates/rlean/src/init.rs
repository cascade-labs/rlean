use anyhow::{bail, Result};

use crate::config::{GlobalConfig, WorkspaceConfig};

#[derive(clap::Args)]
pub struct InitArgs {
    /// Default strategy language
    #[arg(long, default_value = "python", value_parser = ["python", "csharp"])]
    pub language: String,

    /// Path to the historical data provider to use by default
    #[arg(long, value_parser = ["polygon", "thetadata", "local"])]
    pub data_provider: Option<String>,
}

pub fn run_init(args: InitArgs) -> Result<()> {
    let workspace = std::env::current_dir()?;

    // Refuse to re-init an already-initialised workspace.
    if workspace.join("rlean.json").exists() {
        bail!(
            "rlean.json already exists in {}.\n\
             If you want to re-initialise, delete rlean.json first.",
            workspace.display()
        );
    }

    // ── rlean.json ─────────────────────────────────────────────────────────────
    let ws_config = WorkspaceConfig {
        data_folder: "data".to_string(),
        default_language: args.language.clone(),
    };
    ws_config.save(&workspace)?;
    println!("Created rlean.json");

    // ── data/ ─────────────────────────────────────────────────────────────────
    let data_dir = workspace.join("data");
    std::fs::create_dir_all(&data_dir)?;
    println!("Created data/");

    // ── ~/.rlean/config ───────────────────────────────────────────────────────
    let mut global = GlobalConfig::load()?;
    global.default_language = args.language.clone();
    global.workspace = Some(workspace.display().to_string());
    global.save()?;
    println!("Updated ~/.rlean/config");

    println!();
    println!("Workspace initialised at {}", workspace.display());
    println!("Next: rlean create-project <name>");

    Ok(())
}
