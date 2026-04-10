/// Provider registry / factory for the `rlean` CLI.
///
/// Providers are loaded at runtime via `libloading` from installed plugins
/// located at `~/.rlean/plugins/librlean_plugin_<name>.<dylib|so>`.
/// The only built-in provider is `local` / `""` which reads from the local
/// Parquet store without any network calls.
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use lean_data_providers::{config::ProviderConfig, IHistoryProvider, LocalHistoryProvider, StackedHistoryProvider};

/// Rate-limit settings for the CLI — passed alongside plugin config.
///
/// Plugin-specific config (API keys, URLs, etc.) lives in
/// ~/.rlean/plugin-configs.json and is loaded separately in `load_plugin_provider`.
#[derive(Clone, Default)]
pub struct ProviderArgs {
    pub data_root:            std::path::PathBuf,
    pub polygon_rate:         f64,
    pub thetadata_rate:       f64,
    pub thetadata_concurrent: usize,
}

impl ProviderArgs {
    fn rps_for(&self, provider: &str) -> f64 {
        match provider {
            "thetadata" => if self.thetadata_rate > 0.0 { self.thetadata_rate } else { 4.0 },
            _           => if self.polygon_rate > 0.0 { self.polygon_rate } else { 5.0 },
        }
    }
}

/// Build a historical data provider from a (possibly comma-separated) name
/// string.
///
/// When `names` contains a single provider name the provider is returned
/// directly.  When it contains multiple comma-separated names (e.g.
/// `"thetadata,massive"`) a [`StackedHistoryProvider`] is returned that tries
/// each provider in order, falling back to the next when the current one
/// returns no data.
///
/// Provider names:
/// - `"massive"`   — Massive.com historical data (installed plugin)
/// - `"thetadata"` — ThetaData historical data (installed plugin)
/// - `"local"` / `""` — local Parquet store only, no network calls
pub fn build_history_provider(
    names: &str,
    args: ProviderArgs,
) -> Result<Arc<dyn IHistoryProvider>> {
    let provider_names: Vec<&str> = names.split(',').map(str::trim).collect();

    let providers: Vec<Arc<dyn IHistoryProvider>> = provider_names
        .into_iter()
        .map(|name| build_single_provider(name, &args))
        .collect::<Result<_>>()?;

    if providers.len() == 1 {
        Ok(providers.into_iter().next().unwrap())
    } else {
        Ok(Arc::new(StackedHistoryProvider::new(providers)))
    }
}

/// Build a single named provider, drawing credentials from `args`.
fn build_single_provider(
    name: &str,
    args: &ProviderArgs,
) -> Result<Arc<dyn IHistoryProvider>> {
    match name {
        "local" | "" => {
            let config = ProviderConfig {
                data_root: args.data_root.clone(),
                ..Default::default()
            };
            Ok(Arc::new(LocalHistoryProvider::new(&config.data_root)))
        }
        name => load_plugin_provider(name, args),
    }
}

/// Load a history provider from an installed plugin dylib.
///
/// The plugin is expected to export `rlean_create_history_provider` with the
/// signature defined in `lean_plugin::CreateHistoryProviderFn`.
fn load_plugin_provider(name: &str, args: &ProviderArgs) -> Result<Arc<dyn IHistoryProvider>> {
    use libloading::{Library, Symbol};

    let lib_name = format!(
        "librlean_plugin_{}.{}",
        name.replace('-', "_"),
        dylib_ext()
    );
    let plugin_path = home_dir()?.join(".rlean").join("plugins").join(&lib_name);

    if !plugin_path.exists() {
        bail!(
            "Plugin '{}' is not installed. Run: rlean plugin install {}",
            name,
            name
        );
    }

    let max_concurrent = if args.thetadata_concurrent > 0 { args.thetadata_concurrent } else { 4 };

    // Start with stored plugin config from ~/.rlean/plugin-configs.json.
    // This lets users set plugin-specific keys (e.g. api_key, base_url) via
    // `rlean config set <plugin>.<key> <value>`.
    use crate::config::PluginConfigs;
    let plugin_configs = PluginConfigs::load().unwrap_or_default();
    let mut plugin_cfg = plugin_configs.get_plugin(name);

    // Add rlean-managed fields (do not overwrite if the plugin explicitly set them).
    plugin_cfg
        .entry("data_root".to_string())
        .or_insert_with(|| serde_json::json!(args.data_root.display().to_string()));
    plugin_cfg
        .entry("requests_per_second".to_string())
        .or_insert_with(|| serde_json::json!(args.rps_for(name)));
    plugin_cfg
        .entry("max_concurrent".to_string())
        .or_insert_with(|| serde_json::json!(max_concurrent));



    let config_json = serde_json::Value::Object(plugin_cfg).to_string();

    // Leak the library so it lives for the process lifetime.
    // Providers are long-lived (process-scoped) so this is intentional.
    let lib = Box::leak(Box::new(unsafe { Library::new(&plugin_path) }.with_context(|| {
        format!("Failed to load plugin library: {}", plugin_path.display())
    })?));

    let create: Symbol<unsafe extern "C" fn(*const std::os::raw::c_char) -> *mut ()> =
        unsafe { lib.get(b"rlean_create_history_provider\0") }.map_err(|_| {
            anyhow::anyhow!(
                "Plugin '{}' does not export rlean_create_history_provider",
                name
            )
        })?;

    let config_cstr = std::ffi::CString::new(config_json)?;
    let raw = unsafe { create(config_cstr.as_ptr()) };

    if raw.is_null() {
        bail!(
            "Plugin '{}' returned null from rlean_create_history_provider",
            name
        );
    }

    let provider: Arc<dyn IHistoryProvider> =
        unsafe { *Box::from_raw(raw as *mut Arc<dyn IHistoryProvider>) };

    Ok(provider)
}

fn dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") { "dylib" } else { "so" }
}

fn home_dir() -> Result<std::path::PathBuf> {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(std::path::PathBuf::from))
        .context("HOME env not set")
}
