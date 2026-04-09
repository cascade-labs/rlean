/// `rlean config` — get/set/list workspace and credential configuration
///
/// API keys are stored in ~/.rlean/credentials (never in lean.json).
/// Workspace settings are stored in ~/.rlean/config and lean.json.
///
/// Known keys:
///   polygon-api-key          Polygon.io API key
///   thetadata-api-key        ThetaData API key
///   default-language         python | csharp
///   data-folder              Parquet data root (relative to lean.json)
use anyhow::{bail, Result};

use crate::config::{Credentials, GlobalConfig, WorkspaceConfig};

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(clap::Subcommand)]
pub enum ConfigCommand {
    /// Set a configuration value
    Set {
        /// Config key (e.g. polygon-api-key, thetadata-api-key, default-language)
        key: String,
        /// Value to set
        value: String,
    },
    /// Get a configuration value
    Get {
        /// Config key
        key: String,
    },
    /// List all configuration values
    List,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_config(args: ConfigArgs) -> Result<()> {
    match args.command {
        ConfigCommand::Set { key, value } => cmd_set(&key, &value),
        ConfigCommand::Get { key }        => cmd_get(&key),
        ConfigCommand::List               => cmd_list(),
    }
}

// ── Commands ──────────────────────────────────────────────────────────────────

fn cmd_set(key: &str, value: &str) -> Result<()> {
    match key {
        "polygon-api-key" => {
            let mut creds = Credentials::load()?;
            creds.polygon_api_key = Some(value.to_string());
            creds.save()?;
            println!("Set polygon-api-key in ~/.rlean/credentials");
        }
        "thetadata-api-key" => {
            let mut creds = Credentials::load()?;
            creds.thetadata_api_key = Some(value.to_string());
            creds.save()?;
            println!("Set thetadata-api-key in ~/.rlean/credentials");
        }
        "default-language" => {
            if value != "python" && value != "csharp" {
                bail!("default-language must be python or csharp, got '{}'", value);
            }
            let mut cfg = GlobalConfig::load()?;
            cfg.default_language = value.to_string();
            cfg.save()?;
            // Also update lean.json if present in cwd
            let ws = std::env::current_dir()?;
            if ws.join("lean.json").exists() {
                let mut ws_cfg = WorkspaceConfig::load(&ws)?;
                ws_cfg.default_language = value.to_string();
                ws_cfg.save(&ws)?;
            }
            println!("Set default-language = {value}");
        }
        "data-folder" => {
            let ws = std::env::current_dir()?;
            if !ws.join("lean.json").exists() {
                bail!("No lean.json in current directory. Run `rlean init` first.");
            }
            let mut ws_cfg = WorkspaceConfig::load(&ws)?;
            ws_cfg.data_folder = value.to_string();
            ws_cfg.save(&ws)?;
            println!("Set data-folder = {value} in lean.json");
        }
        _ => bail!(
            "Unknown key '{}'. Known keys: polygon-api-key, thetadata-api-key, \
             default-language, data-folder",
            key
        ),
    }
    Ok(())
}

fn cmd_get(key: &str) -> Result<()> {
    match key {
        "polygon-api-key" => {
            let creds = Credentials::load()?;
            match &creds.polygon_api_key {
                Some(v) => println!("{}", mask(v)),
                None    => println!("(not set)"),
            }
        }
        "thetadata-api-key" => {
            let creds = Credentials::load()?;
            match &creds.thetadata_api_key {
                Some(v) => println!("{}", mask(v)),
                None    => println!("(not set)"),
            }
        }
        "default-language" => {
            let cfg = GlobalConfig::load()?;
            println!("{}", cfg.default_language);
        }
        "data-folder" => {
            let ws = std::env::current_dir()?;
            let cfg = WorkspaceConfig::load(&ws)?;
            println!("{}", cfg.data_folder);
        }
        _ => bail!(
            "Unknown key '{}'. Known keys: polygon-api-key, thetadata-api-key, \
             default-language, data-folder",
            key
        ),
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let creds  = Credentials::load()?;
    let global = GlobalConfig::load()?;
    let ws     = std::env::current_dir()?;
    let ws_cfg = WorkspaceConfig::load(&ws).ok();

    println!("{:<22} {}", "KEY", "VALUE");
    println!("{}", "-".repeat(50));

    println!(
        "{:<22} {}",
        "polygon-api-key",
        creds.polygon_api_key.as_deref().map(mask).unwrap_or("(not set)".to_string())
    );
    println!(
        "{:<22} {}",
        "thetadata-api-key",
        creds.thetadata_api_key.as_deref().map(mask).unwrap_or("(not set)".to_string())
    );
    println!("{:<22} {}", "default-language", global.default_language);

    if let Some(ws_cfg) = ws_cfg {
        println!("{:<22} {}", "data-folder", ws_cfg.data_folder);
    } else {
        println!("{:<22} (no lean.json in cwd)", "data-folder");
    }

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Show first 4 chars + asterisks for API keys.
fn mask(s: &str) -> String {
    if s.len() <= 4 {
        return "*".repeat(s.len());
    }
    format!("{}{}",  &s[..4], "*".repeat(s.len() - 4))
}
