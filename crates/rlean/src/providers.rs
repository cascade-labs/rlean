/// Provider registry / factory for the `rlean` CLI.
///
/// Providers are loaded at runtime via `libloading` from installed plugins
/// located at `~/.rlean/plugins/librlean_plugin_<name>.<dylib|so>`.
/// The only built-in provider is `local` / `""` which reads from the local
/// Parquet store without any network calls.
///
/// Plugin dylibs are loaded **lazily** — the `Library::new` call happens only
/// when the first actual data fetch is needed.  This means `--data-provider-historical`
/// is safe to pass even when all data is cached: if `pre_fetch_all` finds
/// existing Parquet files it skips fetching entirely and the dylibs are
/// never loaded.
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;

use lean_core::{DateTime, Resolution};
use lean_data::DataQueueHandler;
use lean_data::{CustomDataConfig, CustomDataPoint, CustomDataQuery, CustomDataSource};
use lean_data_providers::{
    CustomDataContext, IHistoryProvider, LocalHistoryProvider, StackedHistoryProvider,
};
use lean_live::DataQueueHandlerManager;
use lean_storage::IcebergStore;

/// A lazy wrapper around a named plugin provider.
///
/// The dylib is not loaded until the first call to `get_history`.
struct LazyPluginProvider {
    name: String,
    args: ProviderArgs,
    inner: OnceLock<Result<Arc<dyn IHistoryProvider>, String>>,
}

struct LazyCustomDataSource {
    name: String,
    path: PathBuf,
    data_root: PathBuf,
    plugin_config: serde_json::Map<String, serde_json::Value>,
    inner: OnceLock<Result<Arc<dyn lean_data_providers::ICustomDataSource>, String>>,
}

impl LazyCustomDataSource {
    fn new(
        name: String,
        path: PathBuf,
        data_root: PathBuf,
        plugin_config: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        Self {
            name,
            path,
            data_root,
            plugin_config,
            inner: OnceLock::new(),
        }
    }

    fn get(&self) -> anyhow::Result<Arc<dyn lean_data_providers::ICustomDataSource>> {
        self.inner
            .get_or_init(|| {
                load_custom_data_source(
                    &self.path,
                    &self.name,
                    &self.data_root,
                    self.plugin_config.clone(),
                )
                .map_err(|e| format!("{e:#}"))
            })
            .clone()
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

impl lean_data_providers::ICustomDataSource for LazyCustomDataSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn get_source(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        config: &CustomDataConfig,
    ) -> Option<CustomDataSource> {
        self.get()
            .ok()
            .and_then(|source| source.get_source(ticker, date, config))
    }

    fn get_live_points(
        &self,
        ticker: &str,
        utc_time: DateTime,
        config: &CustomDataConfig,
        query: &CustomDataQuery,
    ) -> Option<Vec<lean_data::CustomDataPoint>> {
        self.get()
            .ok()
            .and_then(|source| source.get_live_points(ticker, utc_time, config, query))
    }

    fn live_poll_delay(
        &self,
        ticker: &str,
        utc_time: DateTime,
        source_available: bool,
        config: &CustomDataConfig,
        query: &CustomDataQuery,
    ) -> Duration {
        self.get()
            .ok()
            .map(|source| source.live_poll_delay(ticker, utc_time, source_available, config, query))
            .unwrap_or_else(|| Duration::from_secs(300))
    }

    fn reader(
        &self,
        line: &str,
        date: chrono::NaiveDate,
        config: &CustomDataConfig,
    ) -> Option<CustomDataPoint> {
        self.get()
            .ok()
            .and_then(|source| source.reader(line, date, config))
    }

    fn default_resolution(&self) -> Resolution {
        self.get()
            .ok()
            .map(|source| source.default_resolution())
            .unwrap_or(Resolution::Daily)
    }

    fn default_resolution_for_ticker(&self, ticker: &str) -> Resolution {
        self.get()
            .ok()
            .map(|source| source.default_resolution_for_ticker(ticker))
            .unwrap_or(Resolution::Daily)
    }

    fn requires_mapping(&self) -> bool {
        self.get()
            .ok()
            .map(|source| source.requires_mapping())
            .unwrap_or(false)
    }

    fn is_parquet_native(&self) -> bool {
        self.get()
            .ok()
            .map(|source| source.is_parquet_native())
            .unwrap_or(false)
    }

    fn is_full_history_source(&self) -> bool {
        self.get()
            .ok()
            .map(|source| source.is_full_history_source())
            .unwrap_or(false)
    }

    fn read_history_line(&self, line: &str, config: &CustomDataConfig) -> Option<CustomDataPoint> {
        self.get()
            .ok()
            .and_then(|source| source.read_history_line(line, config))
    }

    fn history(
        &self,
        ticker: &str,
        config: &CustomDataConfig,
    ) -> Option<Result<Vec<CustomDataPoint>, String>> {
        self.get()
            .ok()
            .and_then(|source| source.history(ticker, config))
    }

    fn history_sources(
        &self,
        ticker: &str,
        config: &CustomDataConfig,
    ) -> Option<Result<Vec<(chrono::NaiveDate, CustomDataSource)>, String>> {
        self.get()
            .ok()
            .and_then(|source| source.history_sources(ticker, config))
    }
}

impl LazyPluginProvider {
    fn new(name: &str, args: ProviderArgs) -> Self {
        Self {
            name: name.to_string(),
            args,
            inner: OnceLock::new(),
        }
    }

    fn get(&self) -> anyhow::Result<Arc<dyn IHistoryProvider>> {
        let result = self.inner.get_or_init(|| {
            load_plugin_provider(&self.name, &self.args).map_err(|e| format!("{e:#}"))
        });
        result.clone().map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[async_trait]
impl IHistoryProvider for LazyPluginProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn get_history(
        &self,
        request: &lean_data_providers::HistoryRequest,
    ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_history(request).await
    }

    async fn get_quote_bars(
        &self,
        request: &lean_data_providers::HistoryRequest,
    ) -> anyhow::Result<Vec<lean_data::QuoteBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_quote_bars(request).await
    }

    async fn get_ticks(
        &self,
        request: &lean_data_providers::HistoryRequest,
    ) -> anyhow::Result<Vec<lean_data::Tick>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_ticks(request).await
    }

    async fn get_margin_interest_rates(
        &self,
        request: &lean_data_providers::HistoryRequest,
    ) -> anyhow::Result<Vec<lean_data::MarginInterestRate>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_margin_interest_rates(request).await
    }

    async fn get_factor_file(
        &self,
        symbol: &lean_core::Symbol,
    ) -> anyhow::Result<Vec<lean_storage::FactorFileEntry>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_factor_file(symbol).await
    }

    async fn get_map_file(
        &self,
        symbol: &lean_core::Symbol,
    ) -> anyhow::Result<Vec<lean_storage::MapFileEntry>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_map_file(symbol).await
    }

    async fn get_history_batch(
        &self,
        request: &lean_data_providers::HistoryBatchRequest,
    ) -> anyhow::Result<lean_data_providers::MarketDataBatch> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_history_batch(request).await
    }

    async fn get_option_history_batch(
        &self,
        request: &lean_data_providers::OptionHistoryBatchRequest,
    ) -> anyhow::Result<lean_data_providers::OptionMarketDataBatch> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_option_history_batch(request).await
    }

    async fn get_option_eod_bars(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_storage::OptionEodBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_option_eod_bars(ticker, date).await
    }

    async fn get_option_universe(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_storage::OptionUniverseRow>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_option_universe(ticker, date).await
    }

    async fn get_option_trade_bars(
        &self,
        ticker: &str,
        resolution: lean_core::Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider
            .get_option_trade_bars(ticker, resolution, date)
            .await
    }

    async fn get_option_trade_bars_filtered(
        &self,
        ticker: &str,
        resolution: lean_core::Resolution,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<lean_data::TradeBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider
            .get_option_trade_bars_filtered(ticker, resolution, date, contracts)
            .await
    }

    async fn get_option_quote_bars(
        &self,
        ticker: &str,
        resolution: lean_core::Resolution,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_data::QuoteBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider
            .get_option_quote_bars(ticker, resolution, date)
            .await
    }

    async fn get_option_quote_bars_filtered(
        &self,
        ticker: &str,
        resolution: lean_core::Resolution,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<lean_data::QuoteBar>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider
            .get_option_quote_bars_filtered(ticker, resolution, date, contracts)
            .await
    }

    async fn get_option_ticks(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> anyhow::Result<Vec<lean_data::Tick>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider.get_option_ticks(ticker, date).await
    }

    async fn get_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<Vec<lean_data::Tick>> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider
            .get_option_ticks_filtered(ticker, date, contracts)
            .await
    }

    async fn stream_option_ticks_filtered(
        &self,
        ticker: &str,
        date: chrono::NaiveDate,
        contracts: &[lean_storage::OptionUniverseRow],
    ) -> anyhow::Result<lean_data_providers::TickStream> {
        let provider = self.get().map_err(|e| anyhow::anyhow!("{e}"))?;
        provider
            .stream_option_ticks_filtered(ticker, date, contracts)
            .await
    }

    fn earliest_date(&self) -> Option<chrono::NaiveDate> {
        self.get()
            .ok()
            .and_then(|provider| provider.earliest_date())
    }
}

/// Data-root and store settings for the CLI — passed alongside plugin config.
///
/// Plugin-specific config (API keys, URLs, rate limits, etc.) lives in
/// ~/.rlean/plugin-configs.json and is loaded separately in `load_plugin_provider`.
#[derive(Clone)]
pub struct ProviderArgs {
    pub data_root: std::path::PathBuf,
    /// The market-data store, always the REST Iceberg catalog. Required: every
    /// run resolves the catalog before building providers.
    pub data_store: Arc<IcebergStore>,
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
///
/// A `LocalHistoryProvider` is prepended as the highest-priority provider
/// unless `local` is already listed first. Local is expected to return data
/// only when it can satisfy the request; otherwise remote providers are tried.
pub fn build_history_provider(
    names: &str,
    args: ProviderArgs,
) -> Result<Arc<dyn IHistoryProvider>> {
    let provider_names: Vec<&str> = names.split(',').map(str::trim).collect();

    let has_local_first = matches!(provider_names.first(), Some(&"local") | Some(&""));
    // When we auto-prepend local in front of remote providers, it must report a
    // miss for windows it cannot fully cover so the remotes backfill the gap
    // (e.g. indicator warmup reaching before the cached history). A local-only
    // stack keeps best-effort behavior.
    let mut providers: Vec<Arc<dyn IHistoryProvider>> = if !has_local_first {
        vec![build_local_history_provider_with_strict_coverage(
            &args, true,
        )]
    } else {
        vec![]
    };

    providers.extend(
        provider_names
            .into_iter()
            .map(|name| build_single_provider(name, &args))
            .collect::<Result<Vec<_>>>()?,
    );

    if providers.len() == 1 {
        Ok(providers.into_iter().next().unwrap())
    } else {
        Ok(Arc::new(StackedHistoryProvider::new(providers)))
    }
}

fn build_local_history_provider(args: &ProviderArgs) -> Arc<dyn IHistoryProvider> {
    build_local_history_provider_with_strict_coverage(args, false)
}

fn build_local_history_provider_with_strict_coverage(
    args: &ProviderArgs,
    strict: bool,
) -> Arc<dyn IHistoryProvider> {
    Arc::new(LocalHistoryProvider::from_store(args.data_store.clone()).with_strict_coverage(strict))
}

/// Build the stacked live data queue handler set for a live deployment.
///
/// Provider order is significant: the first handler accepting a subscription
/// owns it, matching LEAN's `DataQueueHandlerManager`.
pub fn build_live_data_queue(names: &str, args: ProviderArgs) -> Result<DataQueueHandlerManager> {
    let handlers = names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| load_plugin_live_data_provider(name, &args))
        .collect::<Result<Vec<_>>>()?;

    if handlers.is_empty() {
        bail!("no live data providers were specified");
    }

    Ok(DataQueueHandlerManager::new(handlers))
}

/// Load a brokerage plugin from an installed dylib.
///
/// Paper brokerage is built into `lean-brokerages`; this loader is for
/// real-money brokerage plugins that export `rlean_create_brokerage`.
pub fn load_brokerage_plugin(
    name: &str,
    args: &ProviderArgs,
) -> Result<Box<dyn lean_brokerages::Brokerage>> {
    use libloading::{Library, Symbol};

    let lib_name = format!("librlean_plugin_{}.{}", name.replace('-', "_"), dylib_ext());
    let plugin_path = home_dir()?.join(".rlean").join("plugins").join(&lib_name);
    if !plugin_path.exists() {
        bail!(
            "Brokerage plugin '{}' is not installed. Run: rlean plugin install {}",
            name,
            name
        );
    }

    let config_json = plugin_config_json(name, args)?;
    let lib = Box::leak(Box::new(
        unsafe { Library::new(&plugin_path) }
            .with_context(|| format!("Failed to load plugin library: {}", plugin_path.display()))?,
    ));
    initialize_plugin_library(lib, name);
    let create: Symbol<lean_plugin::CreateBrokerageFn> = unsafe {
        lib.get(b"rlean_create_brokerage\0")
    }
    .map_err(|_| anyhow::anyhow!("Plugin '{}' does not export rlean_create_brokerage", name))?;

    let config_cstr = std::ffi::CString::new(config_json)?;
    let raw = unsafe { create(config_cstr.as_ptr()) };
    if raw.is_null() {
        bail!(
            "Plugin '{}' returned null from rlean_create_brokerage",
            name
        );
    }

    let brokerage: Box<dyn lean_brokerages::Brokerage> =
        unsafe { *Box::from_raw(raw as *mut Box<dyn lean_brokerages::Brokerage>) };
    Ok(brokerage)
}

/// Build a single named provider, drawing credentials from `args`.
///
/// Plugin providers are returned as [`LazyPluginProvider`] wrappers so the
/// dylib is not loaded until a fetch is actually needed.
fn build_single_provider(name: &str, args: &ProviderArgs) -> Result<Arc<dyn IHistoryProvider>> {
    match name {
        "local" | "" => Ok(build_local_history_provider(args)),
        name => {
            // Validate plugin path exists before accepting the name, so the user
            // gets a clear error at startup rather than at first fetch.
            let lib_name = format!("librlean_plugin_{}.{}", name.replace('-', "_"), dylib_ext());
            let plugin_path = home_dir()?.join(".rlean").join("plugins").join(&lib_name);
            if !plugin_path.exists() {
                bail!(
                    "Plugin '{}' is not installed. Run: rlean plugin install {}",
                    name,
                    name
                );
            }
            Ok(Arc::new(LazyPluginProvider::new(name, args.clone())))
        }
    }
}

/// Load a history provider from an installed plugin dylib.
///
/// The plugin is expected to export `rlean_create_history_provider` with the
/// signature defined in `lean_plugin::CreateHistoryProviderFn`.
fn load_plugin_provider(name: &str, args: &ProviderArgs) -> Result<Arc<dyn IHistoryProvider>> {
    use libloading::{Library, Symbol};

    let lib_name = format!("librlean_plugin_{}.{}", name.replace('-', "_"), dylib_ext());
    let plugin_path = home_dir()?.join(".rlean").join("plugins").join(&lib_name);

    if !plugin_path.exists() {
        bail!(
            "Plugin '{}' is not installed. Run: rlean plugin install {}",
            name,
            name
        );
    }

    let config_json = plugin_config_json(name, args)?;

    // Leak the library so it lives for the process lifetime.
    // Providers are long-lived (process-scoped) so this is intentional.
    let lib = Box::leak(Box::new(
        unsafe { Library::new(&plugin_path) }
            .with_context(|| format!("Failed to load plugin library: {}", plugin_path.display()))?,
    ));
    initialize_plugin_library(lib, name);

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

fn load_plugin_live_data_provider(
    name: &str,
    args: &ProviderArgs,
) -> Result<Box<dyn DataQueueHandler>> {
    use libloading::{Library, Symbol};

    let lib_name = format!("librlean_plugin_{}.{}", name.replace('-', "_"), dylib_ext());
    let plugin_path = home_dir()?.join(".rlean").join("plugins").join(&lib_name);

    if !plugin_path.exists() {
        bail!(
            "Live data plugin '{}' is not installed. Run: rlean plugin install {}",
            name,
            name
        );
    }

    let config_json = plugin_config_json(name, args)?;
    let lib = Box::leak(Box::new(
        unsafe { Library::new(&plugin_path) }
            .with_context(|| format!("Failed to load plugin library: {}", plugin_path.display()))?,
    ));
    initialize_plugin_library(lib, name);

    let create: Symbol<lean_plugin::CreateLiveDataProviderFn> =
        unsafe { lib.get(b"rlean_create_live_data_provider\0") }.map_err(|_| {
            anyhow::anyhow!(
                "Plugin '{}' does not export rlean_create_live_data_provider",
                name
            )
        })?;

    let config_cstr = std::ffi::CString::new(config_json)?;
    let raw = unsafe { create(config_cstr.as_ptr()) };
    if raw.is_null() {
        bail!(
            "Plugin '{}' returned null from rlean_create_live_data_provider",
            name
        );
    }

    let provider: Box<dyn DataQueueHandler> =
        unsafe { *Box::from_raw(raw as *mut Box<dyn DataQueueHandler>) };
    Ok(provider)
}

fn plugin_config_json(name: &str, args: &ProviderArgs) -> Result<String> {
    use crate::config::PluginConfigs;
    let plugin_configs = PluginConfigs::load().unwrap_or_default();
    let mut plugin_cfg = plugin_configs.get_plugin(name);

    plugin_cfg
        .entry("data_root".to_string())
        .or_insert_with(|| serde_json::json!(args.data_root.display().to_string()));

    Ok(serde_json::Value::Object(plugin_cfg).to_string())
}

/// Scan `~/.rlean/plugins/` for installed plugin filenames and return lazy
/// custom-data wrappers. The actual plugin library is not opened until a
/// strategy subscribes to that source type.
pub fn load_custom_data_plugins(
    data_root: &std::path::Path,
) -> Vec<Arc<dyn lean_data_providers::ICustomDataSource>> {
    let plugins_dir = match home_dir() {
        Ok(h) => h.join(".rlean").join("plugins"),
        Err(_) => return vec![],
    };

    if !plugins_dir.exists() {
        return vec![];
    }

    let pattern = plugins_dir.join(format!("*.{}", dylib_ext()));
    let glob_pattern = pattern.to_string_lossy().to_string();

    let mut sources: Vec<Arc<dyn lean_data_providers::ICustomDataSource>> = Vec::new();
    let plugin_configs = crate::config::PluginConfigs::load().unwrap_or_default();

    let paths: Vec<_> = match glob::glob(&glob_pattern) {
        Ok(paths) => paths.filter_map(|r| r.ok()).collect(),
        Err(_) => return vec![],
    };

    for path in paths {
        let Some(source_name) = plugin_name_from_library_path(&path) else {
            continue;
        };
        let plugin_config = plugin_configs.get_plugin(&source_name);
        sources.push(Arc::new(LazyCustomDataSource::new(
            source_name.clone(),
            path.clone(),
            data_root.to_path_buf(),
            plugin_config,
        )));
        tracing::debug!(
            "Registered lazy custom data plugin candidate: {}",
            source_name
        );
    }

    sources
}

fn plugin_name_from_library_path(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let name = stem.strip_prefix("librlean_plugin_")?;
    Some(name.to_string())
}

fn load_custom_data_source(
    path: &std::path::Path,
    expected_name: &str,
    data_root: &std::path::Path,
    plugin_config: serde_json::Map<String, serde_json::Value>,
) -> Result<Arc<dyn lean_data_providers::ICustomDataSource>> {
    use libloading::{Library, Symbol};

    let lib = Box::leak(Box::new(unsafe { Library::new(path) }.with_context(
        || format!("Failed to load custom data plugin: {}", path.display()),
    )?));
    initialize_plugin_library(lib, &path.display().to_string());

    let factory: Symbol<unsafe extern "C" fn() -> *mut ()> =
        unsafe { lib.get(b"rlean_custom_data_factory\0") }.map_err(|_| {
            anyhow::anyhow!(
                "Plugin '{}' does not export rlean_custom_data_factory",
                path.display()
            )
        })?;

    let raw = unsafe { factory() };
    if raw.is_null() {
        bail!(
            "Plugin {} returned null from rlean_custom_data_factory",
            path.display()
        );
    }

    let mut source_box: Box<dyn lean_data_providers::ICustomDataSource> =
        unsafe { *Box::from_raw(raw as *mut Box<dyn lean_data_providers::ICustomDataSource>) };
    let source_name = source_box.name().to_string();
    if !source_name.eq_ignore_ascii_case(expected_name) {
        tracing::warn!(
            "Custom data plugin descriptor name '{}' differs from source name '{}'",
            expected_name,
            source_name
        );
    }
    let context = CustomDataContext::with_plugin_config(data_root, plugin_config);
    source_box.initialize(&context);
    Ok(Arc::from(source_box))
}

fn initialize_plugin_library(lib: &libloading::Library, requested_plugin: &str) {
    let _ = plugin_descriptor(lib, requested_plugin);
}

fn plugin_descriptor(
    lib: &libloading::Library,
    requested_plugin: &str,
) -> Option<lean_plugin::PluginDescriptor> {
    type DescriptorFn = unsafe extern "C" fn() -> lean_plugin::PluginDescriptor;

    let descriptor: libloading::Symbol<DescriptorFn> =
        match unsafe { lib.get(b"rlean_plugin_descriptor\0") } {
            Ok(descriptor) => descriptor,
            Err(error) => {
                tracing::debug!(
                    "Plugin {requested_plugin} does not export rlean_plugin_descriptor: {error}"
                );
                return None;
            }
        };

    let descriptor = unsafe { descriptor() };
    tracing::debug!(
        requested_plugin,
        plugin = descriptor.name_str(),
        version = descriptor.version_str(),
        kind = %descriptor.kind,
        "Initialized plugin descriptor"
    );
    Some(descriptor)
}

fn dylib_ext() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    }
}

fn home_dir() -> Result<std::path::PathBuf> {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(std::path::PathBuf::from))
        .context("HOME env not set")
}
