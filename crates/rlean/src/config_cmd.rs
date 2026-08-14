use crate::config::{
    ensure_known_provider, validate_verglas_database, GlobalConfig, WorkspaceConfig,
    KNOWN_PROVIDERS,
};
/// `rlean config` — get/set/list workspace and provider configuration
///
/// All values are stored in ~/.rlean/config (mode 0600). Workspace language
/// may also update rlean.json in the current directory.
///
/// Known keys:
///   default-language            python | csharp
///   verglas_endpoint            Verglas SDK gateway (e.g. http://127.0.0.1:8334)
///   verglas_access_uri          Verglas access service (e.g. http://127.0.0.1:8345)
///   verglas_database            Named Verglas lakehouse database
///   verglas_token               Verglas bearer token for all discovered services
///   <provider>.<key>            Provider credentials (e.g. thetadata.api_key)
///                               Known providers: thetadata, massive, tradier, fred
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
    // Dotted keys configure a named native provider.
    if let Some((provider, subkey)) = key.split_once('.') {
        ensure_known_provider(provider)?;
        let mut cfg = GlobalConfig::load()?;
        cfg.set_provider_key(provider, subkey, value.to_string())?;
        cfg.save()?;
        println!("Set {provider}.{subkey} in ~/.rlean/config");
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
        "verglas_endpoint" | "verglas_access_uri" | "verglas_database" | "verglas_token" => {
            if key == "verglas_database" {
                validate_verglas_database(value)?;
            }
            let mut cfg = GlobalConfig::load()?;
            set_verglas_key(&mut cfg, key, value.to_string())?;
            cfg.save()?;
            if key == "verglas_token" {
                println!("Set {key} in ~/.rlean/config");
            } else {
                println!("Set {key} = {value} in ~/.rlean/config");
            }
        }
        _ => bail!("{}", unknown_key_message(key)),
    }
    Ok(())
}

fn cmd_get(key: &str) -> Result<()> {
    if let Some((provider, subkey)) = key.split_once('.') {
        ensure_known_provider(provider)?;
        let cfg = GlobalConfig::load()?;
        match cfg.get_provider(provider).get(subkey) {
            Some(s) => println!("{}", mask(s)),
            None => println!("(not set)"),
        }
        return Ok(());
    }

    match key {
        "default-language" => {
            let cfg = GlobalConfig::load()?;
            println!("{}", cfg.default_language);
        }
        "verglas_endpoint" | "verglas_access_uri" | "verglas_database" | "verglas_token" => {
            let cfg = GlobalConfig::load()?;
            match get_verglas_key(&cfg, key)? {
                Some(value) if key == "verglas_token" => println!("{}", mask(value)),
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
    println!("{:<30} VALUE", "KEY");
    println!("{}", "-".repeat(60));

    println!("{:<30} {}", "default-language", global.default_language);
    if let Some(endpoint) = global.verglas_endpoint.as_deref() {
        println!("{:<30} {}", "verglas_endpoint", endpoint);
    }
    if let Some(access_uri) = global.verglas_access_uri.as_deref() {
        println!("{:<30} {}", "verglas_access_uri", access_uri);
    }
    if let Some(database) = global.verglas_database.as_deref() {
        println!("{:<30} {}", "verglas_database", database);
    }
    if let Some(token) = global.verglas_token.as_deref() {
        println!("{:<30} {}", "verglas_token", mask(token));
    }

    for provider in KNOWN_PROVIDERS {
        let section = global.get_provider(provider);
        if section.is_empty() {
            continue;
        }
        for (key, value) in section {
            let display_key = format!("{provider}.{key}");
            println!("{:<30} {}", display_key, mask(&value));
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
        "Unknown key '{key}'. Known keys: default-language, verglas_endpoint, verglas_access_uri, verglas_database, verglas_token. \
         Use <provider>.<key> for provider credentials (e.g. massive.api_key). \
         Known providers: {}.",
        KNOWN_PROVIDERS.join(", ")
    )
}

fn set_verglas_key(cfg: &mut GlobalConfig, key: &str, value: String) -> Result<()> {
    match key {
        "verglas_endpoint" => cfg.verglas_endpoint = Some(value),
        "verglas_access_uri" => cfg.verglas_access_uri = Some(value),
        "verglas_database" => cfg.verglas_database = Some(value),
        "verglas_token" => cfg.verglas_token = Some(value),
        _ => bail!("unknown Verglas config key '{key}'"),
    }
    Ok(())
}

fn get_verglas_key<'a>(cfg: &'a GlobalConfig, key: &str) -> Result<Option<&'a str>> {
    match key {
        "verglas_endpoint" => Ok(cfg.verglas_endpoint.as_deref()),
        "verglas_access_uri" => Ok(cfg.verglas_access_uri.as_deref()),
        "verglas_database" => Ok(cfg.verglas_database.as_deref()),
        "verglas_token" => Ok(cfg.verglas_token.as_deref()),
        _ => bail!("unknown Verglas config key '{key}'"),
    }
}
