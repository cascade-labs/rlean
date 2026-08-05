use crate::config::{GlobalConfig, IntegrationConfigs, WorkspaceConfig};
/// `rlean config` — get/set/list workspace and provider integration configuration
///
/// Integration credentials are stored in ~/.rlean/integration-configs.json.
/// Workspace settings are stored in ~/.rlean/config and rlean.json.
///
/// Known keys:
///   default-language            python | csharp
///   verglas_endpoint            Verglas SDK gateway (e.g. http://127.0.0.1:8334)
///   verglas_token               Verglas bearer token for all discovered services
///   artifact_store              Run artifact relay mode: local | s3 | mirror
///   artifact_s3                 Artifact destination: s3://bucket/prefix
///   artifact_s3_endpoint        Artifact endpoint URL
///   artifact_s3_region          Artifact region
///   artifact_s3_access_key      Artifact access key
///   artifact_s3_secret_key      Artifact secret key
///   <integration>.<key>         Provider integration config (e.g. thetadata.api_key)
use anyhow::{bail, Result};

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
        /// Config key (e.g. default-language, verglas_endpoint, tradier.access_token)
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
    // Dotted keys configure a named provider integration.
    if let Some((integration, subkey)) = key.split_once('.') {
        let mut configs = IntegrationConfigs::load()?;
        configs.set_key(
            integration,
            subkey,
            serde_json::Value::String(value.to_string()),
        );
        configs.save()?;
        println!("Set {integration}.{subkey} in ~/.rlean/integration-configs.json");
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
        "verglas_endpoint" | "verglas_token" => {
            let mut cfg = GlobalConfig::load()?;
            set_verglas_key(&mut cfg, key, value.to_string())?;
            cfg.save()?;
            if key == "verglas_token" {
                println!("Set {key} in ~/.rlean/config");
            } else {
                println!("Set {key} = {value} in ~/.rlean/config");
            }
        }
        "artifact_store" => {
            if rlean_engine::ArtifactStoreMode::parse(value).is_none() {
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
        "artifact_s3"
        | "artifact_s3_endpoint"
        | "artifact_s3_region"
        | "artifact_s3_access_key"
        | "artifact_s3_secret_key" => {
            let mut cfg = GlobalConfig::load()?;
            set_s3_key(&mut cfg, key, value.to_string())?;
            cfg.save()?;
            println!("Set {key} in ~/.rlean/config");
        }
        _ => bail!("{}", unknown_key_message(key)),
    }
    Ok(())
}

fn cmd_get(key: &str) -> Result<()> {
    if let Some((integration, subkey)) = key.split_once('.') {
        let configs = IntegrationConfigs::load()?;
        let integration_cfg = configs.get_integration(integration);
        match integration_cfg.get(subkey) {
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
        "verglas_endpoint" | "verglas_token" => {
            let cfg = GlobalConfig::load()?;
            match get_verglas_key(&cfg, key)? {
                Some(value) if key == "verglas_token" => println!("{}", mask(value)),
                Some(value) => println!("{value}"),
                None => println!("(not set)"),
            }
        }
        "artifact_store" => {
            let cfg = GlobalConfig::load()?;
            println!("{}", cfg.artifact_store.as_deref().unwrap_or("local"));
        }
        "artifact_s3"
        | "artifact_s3_endpoint"
        | "artifact_s3_region"
        | "artifact_s3_access_key"
        | "artifact_s3_secret_key" => {
            let cfg = GlobalConfig::load()?;
            match get_s3_key(&cfg, key)? {
                Some(value) if is_secret_key(key) => println!("{}", mask(value)),
                Some(value) => println!("{value}"),
                None => println!("(not set)"),
            }
        }
        _ => bail!("{}", unknown_key_message(key)),
    }
    Ok(())
}

fn cmd_list() -> Result<()> {
    let global = GlobalConfig::load()?;
    let integration_cfgs = IntegrationConfigs::load()?;
    println!("{:<30} VALUE", "KEY");
    println!("{}", "-".repeat(60));

    println!("{:<30} {}", "default-language", global.default_language);
    if let Some(endpoint) = global.verglas_endpoint.as_deref() {
        println!("{:<30} {}", "verglas_endpoint", endpoint);
    }
    if let Some(token) = global.verglas_token.as_deref() {
        println!("{:<30} {}", "verglas_token", mask(token));
    }
    if let Some(mode) = &global.artifact_store {
        println!("{:<30} {}", "artifact_store", mode);
    }
    for key in [
        "artifact_s3",
        "artifact_s3_endpoint",
        "artifact_s3_region",
        "artifact_s3_access_key",
        "artifact_s3_secret_key",
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

    let mut integration_names: Vec<&str> = integration_cfgs.0.keys().map(String::as_str).collect();
    integration_names.sort();

    if !integration_names.is_empty() {
        println!();
        println!("Provider integration configs (~/.rlean/integration-configs.json):");
        println!("{}", "-".repeat(60));
        for integration in integration_names {
            let cfg = integration_cfgs.get_integration(integration);
            let mut keys: Vec<&str> = cfg.keys().map(String::as_str).collect();
            keys.sort();
            for key in keys {
                let display_key = format!("{integration}.{key}");
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

fn unknown_key_message(key: &str) -> String {
    format!(
        "Unknown key '{key}'. Known keys: default-language, verglas_endpoint, verglas_token, \
         artifact_store, artifact_s3, artifact_s3_endpoint, artifact_s3_region, \
         artifact_s3_access_key, artifact_s3_secret_key. \
         Use <integration>.<key> for provider integration config (e.g. massive.api_key)."
    )
}

fn set_verglas_key(cfg: &mut GlobalConfig, key: &str, value: String) -> Result<()> {
    match key {
        "verglas_endpoint" => cfg.verglas_endpoint = Some(value),
        "verglas_token" => cfg.verglas_token = Some(value),
        _ => bail!("unknown Verglas config key '{key}'"),
    }
    Ok(())
}

fn get_verglas_key<'a>(cfg: &'a GlobalConfig, key: &str) -> Result<Option<&'a str>> {
    match key {
        "verglas_endpoint" => Ok(cfg.verglas_endpoint.as_deref()),
        "verglas_token" => Ok(cfg.verglas_token.as_deref()),
        _ => bail!("unknown Verglas config key '{key}'"),
    }
}

fn set_s3_key(cfg: &mut GlobalConfig, key: &str, value: String) -> Result<()> {
    match key {
        "artifact_s3" => cfg.artifact_s3 = Some(value),
        "artifact_s3_endpoint" => cfg.artifact_s3_endpoint = Some(value),
        "artifact_s3_region" => cfg.artifact_s3_region = Some(value),
        "artifact_s3_access_key" => cfg.artifact_s3_access_key = Some(value),
        "artifact_s3_secret_key" => cfg.artifact_s3_secret_key = Some(value),
        _ => bail!("unknown S3 config key '{key}'"),
    }
    Ok(())
}

fn get_s3_key<'a>(cfg: &'a GlobalConfig, key: &str) -> Result<Option<&'a str>> {
    match key {
        "artifact_s3" => Ok(cfg.artifact_s3.as_deref()),
        "artifact_s3_endpoint" => Ok(cfg.artifact_s3_endpoint.as_deref()),
        "artifact_s3_region" => Ok(cfg.artifact_s3_region.as_deref()),
        "artifact_s3_access_key" => Ok(cfg.artifact_s3_access_key.as_deref()),
        "artifact_s3_secret_key" => Ok(cfg.artifact_s3_secret_key.as_deref()),
        _ => bail!("unknown S3 config key '{key}'"),
    }
}

fn is_secret_key(key: &str) -> bool {
    matches!(key, "artifact_s3_access_key" | "artifact_s3_secret_key")
}
