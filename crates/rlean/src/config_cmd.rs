/// `rlean config` — get/set/list workspace and plugin configuration
///
/// Plugin config is stored per-plugin in ~/.rlean/plugin-configs.json.
/// Workspace settings are stored in ~/.rlean/config and lean.json.
///
/// Known keys:
///   default-language            python | csharp
///   data-folder                 Parquet data root (relative to lean.json)
///   <plugin>.<key>              Plugin-specific config (e.g. thetadata.api_key)
use anyhow::{bail, Result};

use crate::config::{GlobalConfig, PluginConfigs, WorkspaceConfig};

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
    // Dotted keys are plugin config: e.g. "thetadata.api_key"
    if let Some((plugin, subkey)) = key.split_once('.') {
        let mut configs = PluginConfigs::load()?;
        configs.set_key(plugin, subkey, serde_json::Value::String(value.to_string()));
        configs.save()?;
        println!("Set {plugin}.{subkey} in ~/.rlean/plugin-configs.json");
        return Ok(());
    }

    match key {
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
            "Unknown key '{}'. Known keys: default-language, data-folder. \
             Use <plugin>.<key> for plugin config (e.g. thetadata.api_key).",
            key
        ),
    }
    Ok(())
}

fn cmd_get(key: &str) -> Result<()> {
    // Dotted keys are plugin config: e.g. "thetadata.api_key"
    if let Some((plugin, subkey)) = key.split_once('.') {
        let configs = PluginConfigs::load()?;
        let plugin_cfg = configs.get_plugin(plugin);
        match plugin_cfg.get(subkey) {
            Some(serde_json::Value::String(s)) => println!("{}", mask(s)),
            Some(v) => println!("{v}"),
            None => println!("(not set)"),
        }
        return Ok(());
    }

    match key {
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
            "Unknown key '{}'. Known keys: default-language, data-folder. \
             Use <plugin>.<key> for plugin config (e.g. thetadata.api_key).",
            key
        ),
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let global      = GlobalConfig::load()?;
    let ws          = std::env::current_dir()?;
    let ws_cfg      = WorkspaceConfig::load(&ws).ok();
    let plugin_cfgs = PluginConfigs::load()?;

    println!("{:<30} {}", "KEY", "VALUE");
    println!("{}", "-".repeat(60));

    println!("{:<30} {}", "default-language", global.default_language);

    if let Some(ws_cfg) = ws_cfg {
        println!("{:<30} {}", "data-folder", ws_cfg.data_folder);
    } else {
        println!("{:<30} (no lean.json in cwd)", "data-folder");
    }

    // Plugin configs
    let mut plugin_names: Vec<&str> = plugin_cfgs.0.keys().map(String::as_str).collect();
    plugin_names.sort();

    if !plugin_names.is_empty() {
        println!();
        println!("Plugin configs (~/.rlean/plugin-configs.json):");
        println!("{}", "-".repeat(60));
        for plugin in plugin_names {
            let cfg = plugin_cfgs.get_plugin(plugin);
            let mut keys: Vec<&str> = cfg.keys().map(String::as_str).collect();
            keys.sort();
            for key in keys {
                let display_key = format!("{plugin}.{key}");
                let display_val = match cfg.get(key) {
                    Some(serde_json::Value::String(s)) => mask(s),
                    Some(v) => v.to_string(),
                    None => "(not set)".to_string(),
                };
                println!("{:<30} {}", display_key, display_val);
            }
        }
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
