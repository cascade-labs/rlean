/// `rlean config` — get/set/list workspace and plugin configuration
///
/// Plugin config is stored per-plugin in ~/.rlean/plugin-configs.json.
/// Workspace settings are stored in ~/.rlean/config and rlean.json.
///
/// Known keys:
///   default-language            python | csharp
///   datastore                   file | s3
///   data-folder                 Parquet data root (relative to rlean.json)
///   s3_access_key               S3-compatible access key
///   s3_secret_key               S3-compatible secret key
///   s3_bucket                   S3 bucket name
///   s3_endpoint                 S3-compatible endpoint URL
///   s3_region                   S3 region
///   artifact_store              Run artifact relay mode: local | s3 | mirror
///   artifact_s3                 Artifact destination: s3://bucket/prefix
///   artifact_s3_endpoint        Artifact endpoint URL (falls back to s3_endpoint)
///   artifact_s3_region          Artifact region (falls back to s3_region)
///   artifact_s3_access_key      Artifact access key (falls back to s3_access_key)
///   artifact_s3_secret_key      Artifact secret key (falls back to s3_secret_key)
///   data_store                  Market-data store backend: local | s3
///   data_s3                     Market-data warehouse: s3://bucket/prefix
///   data_s3_endpoint            Data-store endpoint URL (falls back to s3_endpoint)
///   data_s3_region              Data-store region (falls back to s3_region)
///   data_s3_access_key          Data-store access key (falls back to s3_access_key)
///   data_s3_secret_key          Data-store secret key (falls back to s3_secret_key)
///   <plugin>.<key>              Plugin-specific config (e.g. thetadata.api_key)
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

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
        /// Config key (e.g. default-language, datastore, thetadata.api_key)
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
        ConfigCommand::Get { key } => cmd_get(&key),
        ConfigCommand::List => cmd_list(),
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
            // Also update rlean.json if present in cwd
            let ws = std::env::current_dir()?;
            if ws.join("rlean.json").exists() {
                let mut ws_cfg = WorkspaceConfig::load(&ws)?;
                ws_cfg.default_language = value.to_string();
                ws_cfg.save(&ws)?;
            }
            println!("Set default-language = {value}");
        }
        "data-folder" => {
            let cwd = std::env::current_dir()?;
            if let Some((workspace, mut cfg)) = crate::config::find_workspace_config(&cwd)? {
                cfg.data_folder = value.to_string();
                cfg.save(&workspace)?;
                println!(
                    "Set data-folder = {value} in {}",
                    workspace.join("rlean.json").display()
                );
            } else {
                let mut cfg = GlobalConfig::load()?;
                cfg.data_folder = Some(value.to_string());
                cfg.save()?;
                println!("Set data-folder = {value} in ~/.rlean/config");
            }
        }
        "datastore" => {
            if value != "file" && value != "s3" {
                bail!("datastore must be file or s3, got '{}'", value);
            }
            let mut cfg = GlobalConfig::load()?;
            cfg.datastore = value.to_string();
            cfg.save()?;
            println!("Set datastore = {value} in ~/.rlean/config");
        }
        "artifact_store" => {
            if lean_engine::ArtifactStoreMode::parse(value).is_none() {
                bail!(
                    "artifact_store must be local, s3, or mirror, got '{}'",
                    value
                );
            }
            let mut cfg = GlobalConfig::load()?;
            cfg.artifact_store = Some(value.to_string());
            cfg.save()?;
            println!("Set artifact_store = {value} in ~/.rlean/config");
        }
        "data_store" => {
            if value != "local" && value != "s3" {
                bail!("data_store must be local or s3, got '{}'", value);
            }
            let mut cfg = GlobalConfig::load()?;
            cfg.data_store = Some(value.to_string());
            cfg.save()?;
            println!("Set data_store = {value} in ~/.rlean/config");
        }
        "s3_access_key"
        | "s3_secret_key"
        | "s3_bucket"
        | "s3_endpoint"
        | "s3_region"
        | "artifact_s3"
        | "artifact_s3_endpoint"
        | "artifact_s3_region"
        | "artifact_s3_access_key"
        | "artifact_s3_secret_key"
        | "data_s3"
        | "data_s3_endpoint"
        | "data_s3_region"
        | "data_s3_access_key"
        | "data_s3_secret_key" => {
            let mut cfg = GlobalConfig::load()?;
            set_s3_key(&mut cfg, key, value.to_string())?;
            cfg.save()?;
            println!("Set {key} in ~/.rlean/config");
        }
        _ => bail!(
            "Unknown key '{}'. Known keys: default-language, datastore, data-folder, \
             s3_access_key, s3_secret_key, s3_bucket, s3_endpoint, s3_region, \
             artifact_store, artifact_s3, artifact_s3_endpoint, artifact_s3_region, \
             artifact_s3_access_key, artifact_s3_secret_key, \
             data_store, data_s3, data_s3_endpoint, data_s3_region, \
             data_s3_access_key, data_s3_secret_key. \
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
            let cwd = std::env::current_dir()?;
            let value = effective_data_folder_display(&cwd)?;
            println!("{value}");
        }
        "datastore" => {
            let cfg = GlobalConfig::load()?;
            println!("{}", cfg.datastore);
        }
        "artifact_store" => {
            let cfg = GlobalConfig::load()?;
            println!("{}", cfg.artifact_store.as_deref().unwrap_or("local"));
        }
        "data_store" => {
            let cfg = GlobalConfig::load()?;
            println!("{}", cfg.data_store.as_deref().unwrap_or("local"));
        }
        "s3_access_key"
        | "s3_secret_key"
        | "s3_bucket"
        | "s3_endpoint"
        | "s3_region"
        | "artifact_s3"
        | "artifact_s3_endpoint"
        | "artifact_s3_region"
        | "artifact_s3_access_key"
        | "artifact_s3_secret_key"
        | "data_s3"
        | "data_s3_endpoint"
        | "data_s3_region"
        | "data_s3_access_key"
        | "data_s3_secret_key" => {
            let cfg = GlobalConfig::load()?;
            match get_s3_key(&cfg, key)? {
                Some(value) if is_secret_key(key) => println!("{}", mask(value)),
                Some(value) => println!("{value}"),
                None => println!("(not set)"),
            }
        }
        _ => bail!(
            "Unknown key '{}'. Known keys: default-language, datastore, data-folder, \
             s3_access_key, s3_secret_key, s3_bucket, s3_endpoint, s3_region, \
             artifact_store, artifact_s3, artifact_s3_endpoint, artifact_s3_region, \
             artifact_s3_access_key, artifact_s3_secret_key, \
             data_store, data_s3, data_s3_endpoint, data_s3_region, \
             data_s3_access_key, data_s3_secret_key. \
             Use <plugin>.<key> for plugin config (e.g. thetadata.api_key).",
            key
        ),
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let global = GlobalConfig::load()?;
    let plugin_cfgs = PluginConfigs::load()?;
    let cwd = std::env::current_dir()?;
    let data_folder = effective_data_folder_display(&cwd)?;

    println!("{:<30} VALUE", "KEY");
    println!("{}", "-".repeat(60));

    println!("{:<30} {}", "default-language", global.default_language);
    println!("{:<30} {}", "datastore", global.datastore);
    println!("{:<30} {}", "data-folder", data_folder);
    if let Some(mode) = &global.artifact_store {
        println!("{:<30} {}", "artifact_store", mode);
    }
    if let Some(mode) = &global.data_store {
        println!("{:<30} {}", "data_store", mode);
    }
    for key in [
        "s3_access_key",
        "s3_secret_key",
        "s3_bucket",
        "s3_endpoint",
        "s3_region",
        "artifact_s3",
        "artifact_s3_endpoint",
        "artifact_s3_region",
        "artifact_s3_access_key",
        "artifact_s3_secret_key",
        "data_s3",
        "data_s3_endpoint",
        "data_s3_region",
        "data_s3_access_key",
        "data_s3_secret_key",
    ] {
        if let Some(value) = get_s3_key(&global, key)? {
            let display = if is_secret_key(key) {
                mask(value)
            } else {
                value.to_string()
            };
            println!("{:<30} {}", key, display);
        }
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
    format!("{}{}", &s[..4], "*".repeat(s.len() - 4))
}

fn effective_data_folder_display(start: &Path) -> Result<String> {
    Ok(crate::config::configured_data_folder(start)?
        .unwrap_or_else(|| PathBuf::from("data"))
        .display()
        .to_string())
}

fn set_s3_key(cfg: &mut GlobalConfig, key: &str, value: String) -> Result<()> {
    match key {
        "s3_access_key" => cfg.s3_access_key = Some(value),
        "s3_secret_key" => cfg.s3_secret_key = Some(value),
        "s3_bucket" => cfg.s3_bucket = Some(value),
        "s3_endpoint" => cfg.s3_endpoint = Some(value),
        "s3_region" => cfg.s3_region = Some(value),
        "artifact_s3" => cfg.artifact_s3 = Some(value),
        "artifact_s3_endpoint" => cfg.artifact_s3_endpoint = Some(value),
        "artifact_s3_region" => cfg.artifact_s3_region = Some(value),
        "artifact_s3_access_key" => cfg.artifact_s3_access_key = Some(value),
        "artifact_s3_secret_key" => cfg.artifact_s3_secret_key = Some(value),
        "data_s3" => cfg.data_s3 = Some(value),
        "data_s3_endpoint" => cfg.data_s3_endpoint = Some(value),
        "data_s3_region" => cfg.data_s3_region = Some(value),
        "data_s3_access_key" => cfg.data_s3_access_key = Some(value),
        "data_s3_secret_key" => cfg.data_s3_secret_key = Some(value),
        _ => bail!("unknown S3 config key '{key}'"),
    }
    Ok(())
}

fn get_s3_key<'a>(cfg: &'a GlobalConfig, key: &str) -> Result<Option<&'a str>> {
    match key {
        "s3_access_key" => Ok(cfg.s3_access_key.as_deref()),
        "s3_secret_key" => Ok(cfg.s3_secret_key.as_deref()),
        "s3_bucket" => Ok(cfg.s3_bucket.as_deref()),
        "s3_endpoint" => Ok(cfg.s3_endpoint.as_deref()),
        "s3_region" => Ok(cfg.s3_region.as_deref()),
        "artifact_s3" => Ok(cfg.artifact_s3.as_deref()),
        "artifact_s3_endpoint" => Ok(cfg.artifact_s3_endpoint.as_deref()),
        "artifact_s3_region" => Ok(cfg.artifact_s3_region.as_deref()),
        "artifact_s3_access_key" => Ok(cfg.artifact_s3_access_key.as_deref()),
        "artifact_s3_secret_key" => Ok(cfg.artifact_s3_secret_key.as_deref()),
        "data_s3" => Ok(cfg.data_s3.as_deref()),
        "data_s3_endpoint" => Ok(cfg.data_s3_endpoint.as_deref()),
        "data_s3_region" => Ok(cfg.data_s3_region.as_deref()),
        "data_s3_access_key" => Ok(cfg.data_s3_access_key.as_deref()),
        "data_s3_secret_key" => Ok(cfg.data_s3_secret_key.as_deref()),
        _ => bail!("unknown S3 config key '{key}'"),
    }
}

fn is_secret_key(key: &str) -> bool {
    matches!(
        key,
        "s3_access_key"
            | "s3_secret_key"
            | "artifact_s3_access_key"
            | "artifact_s3_secret_key"
            | "data_s3_access_key"
            | "data_s3_secret_key"
    )
}
