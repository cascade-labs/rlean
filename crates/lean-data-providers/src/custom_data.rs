use chrono::{NaiveDate, Timelike};
use lean_core::{DateTime, Resolution};
use lean_data::custom::{CustomDataConfig, CustomDataPoint, CustomDataQuery, CustomDataSource};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Engine-owned context passed to custom-data plugins at load time.
///
/// This mirrors LEAN's centralized data-provider model: the engine resolves the
/// data root once, then gives plugins that resolved path instead of requiring
/// each plugin to rediscover it from environment variables or user config.
#[derive(Debug, Clone)]
pub struct CustomDataContext {
    data_root: PathBuf,
    plugin_config: Map<String, Value>,
}

impl CustomDataContext {
    pub fn new(data_root: impl AsRef<Path>) -> Self {
        Self {
            data_root: data_root.as_ref().to_path_buf(),
            plugin_config: Map::new(),
        }
    }

    pub fn with_plugin_config(
        data_root: impl AsRef<Path>,
        plugin_config: Map<String, Value>,
    ) -> Self {
        Self {
            data_root: data_root.as_ref().to_path_buf(),
            plugin_config,
        }
    }

    pub fn data_root(&self) -> &Path {
        &self.data_root
    }

    pub fn plugin_config(&self) -> &Map<String, Value> {
        &self.plugin_config
    }
}

/// Trait implemented by custom data source plugins.
///
/// Custom data history is stored in the engine-owned Iceberg `lean.custom_points` table.
/// Plugins only describe/fetch live source records; they do not expose file layouts.
///
/// # Plugin ABI
///
/// Each plugin is a `cdylib` crate that exports a C function named
/// `rlean_custom_data_factory`:
///
/// ```c
/// // C signature (Rust extern "C" equivalent):
/// // Box<Arc<dyn ICustomDataSource>> *  rlean_custom_data_factory(void);
/// ```
///
/// The factory returns a heap-allocated `Box<Arc<dyn ICustomDataSource>>` cast
/// to `*mut ()`.  The runner loads it with `libloading`, casts the pointer, and
/// calls `Box::from_raw` to take ownership.  A corresponding destroy symbol
/// `rlean_destroy_custom_data_source(ptr: *mut ())` frees the box.
///
/// Because `ICustomDataSource` lives in `lean-data-providers` (not `lean-plugin`),
/// the factory function signature is documented here but **not** type-checked at
/// the ABI boundary — the runner performs the cast internally.
pub trait ICustomDataSource: Send + Sync {
    /// Initialize the plugin with engine-owned services and paths.
    ///
    /// The default keeps older custom-data plugins source-compatible, but new
    /// plugins should use this context for all local file access.
    fn initialize(&mut self, _context: &CustomDataContext) {}

    /// Unique name matching the plugin registry entry (e.g. `"fred"`, `"cboe_vix"`).
    fn name(&self) -> &str;

    /// Return the text data source location for the given ticker and date.
    ///
    /// Return `None` if this date has no data (e.g. weekends for daily sources,
    /// dates before the series started, or providers that expose native parquet).
    fn get_source(
        &self,
        _ticker: &str,
        _date: NaiveDate,
        _config: &CustomDataConfig,
    ) -> Option<CustomDataSource> {
        None
    }

    /// Return decoded in-memory points for live custom-data polling.
    fn get_live_points(
        &self,
        _ticker: &str,
        _utc_time: DateTime,
        _config: &CustomDataConfig,
        _query: &CustomDataQuery,
    ) -> Option<Vec<CustomDataPoint>> {
        None
    }

    /// Delay before the engine asks this provider for live data again.
    ///
    /// Providers with delayed object arrival or market-hours windows can
    /// override this without the framework knowing provider-specific paths or
    /// schedules.
    fn live_poll_delay(
        &self,
        _ticker: &str,
        utc_time: DateTime,
        source_available: bool,
        config: &CustomDataConfig,
        _query: &CustomDataQuery,
    ) -> Duration {
        default_live_poll_delay(utc_time, source_available, config.resolution)
    }

    /// Parse one line/record from the fetched data.
    ///
    /// Return `None` to skip the line (headers, empty lines, comment rows, etc.).
    fn reader(
        &self,
        _line: &str,
        _date: NaiveDate,
        _config: &CustomDataConfig,
    ) -> Option<CustomDataPoint> {
        None
    }

    /// Default resolution for this source.  Overridden when the user calls
    /// `add_data(source_type, ticker, resolution=Resolution.Daily)`.
    fn default_resolution(&self) -> lean_core::Resolution {
        lean_core::Resolution::Daily
    }

    /// Default resolution for a specific data ticker handled by this source.
    ///
    /// Multi-dataset providers can override this when one plugin serves both
    /// daily and intraday datasets.
    fn default_resolution_for_ticker(&self, _ticker: &str) -> lean_core::Resolution {
        self.default_resolution()
    }

    /// Whether the data ticker requires symbol mapping (ticker rename history).
    ///
    /// Almost always `false` for alternative data; set to `true` only for
    /// sources that track equity corporate actions (e.g. equity fundamental data).
    fn requires_mapping(&self) -> bool {
        false
    }

    /// Whether this source returns native Parquet payloads instead of text rows.
    fn is_parquet_native(&self) -> bool {
        false
    }

    /// Returns `true` when `get_source()` always returns the same URL regardless
    /// of date (e.g. FRED, CBOE VIX — a single file contains the full history).
    ///
    /// When `true`, the runner downloads the file **once**, parses all rows with
    /// `read_history_line()`, caches the entire series to a single Parquet file,
    /// and then looks up by date during the backtest loop — no per-day HTTP requests.
    ///
    /// When `false` (default), the existing per-date fetch + per-date Parquet cache
    /// is used.
    fn is_full_history_source(&self) -> bool {
        false
    }

    /// Parse one raw line from a full-history file without filtering by date.
    ///
    /// Called only when `is_full_history_source()` returns `true`.
    /// The returned `CustomDataPoint.time` carries the date parsed from the line
    /// itself.  Return `None` for headers, empty lines, malformed rows, and
    /// missing-value sentinels (e.g. FRED `"."`).
    ///
    /// Default implementation returns `None`; full-history sources must override.
    fn read_history_line(
        &self,
        _line: &str,
        _config: &CustomDataConfig,
    ) -> Option<CustomDataPoint> {
        None
    }

    /// Return a provider-owned full-history series as custom points.
    ///
    /// This is the rlean equivalent of a custom data type resolving an external
    /// source through the subscription framework, but lets plugins model APIs
    /// that do not expose a single line-oriented file. The engine remains
    /// responsible for persisting the returned points to Iceberg.
    fn history(
        &self,
        _ticker: &str,
        _config: &CustomDataConfig,
    ) -> Option<Result<Vec<CustomDataPoint>, String>> {
        None
    }

    /// Return provider source objects for a full custom-data history request.
    ///
    /// The engine fetches each source, decodes it according to
    /// [`CustomDataSource::format`], persists the resulting rows to Iceberg, and
    /// serves subsequent reads from the Iceberg custom-data table.
    fn history_sources(
        &self,
        _ticker: &str,
        _config: &CustomDataConfig,
    ) -> Option<Result<Vec<(NaiveDate, CustomDataSource)>, String>> {
        None
    }
}

/// Type-erased `Arc` wrapper — cloneable for use across threads and `RunConfig`.
pub type ArcCustomDataSource = Arc<dyn ICustomDataSource>;

fn default_live_poll_delay(
    utc_time: DateTime,
    source_available: bool,
    resolution: Resolution,
) -> Duration {
    if source_available {
        return match resolution {
            Resolution::Tick | Resolution::Second => Duration::from_secs(1),
            Resolution::Minute => sleep_until_next_utc_minute(utc_time),
            Resolution::Hour => Duration::from_secs(300),
            Resolution::Daily => Duration::from_secs(300),
        };
    }

    match resolution {
        Resolution::Tick | Resolution::Second | Resolution::Minute => Duration::from_secs(1),
        Resolution::Hour => Duration::from_secs(60),
        Resolution::Daily => Duration::from_secs(300),
    }
}

fn sleep_until_next_utc_minute(utc_time: DateTime) -> Duration {
    let utc = utc_time.to_utc();
    let next = utc + chrono::Duration::minutes(1);
    let Some(next_minute) = next.date_naive().and_hms_opt(next.hour(), next.minute(), 0) else {
        return Duration::from_secs(1);
    };
    let next_utc =
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(next_minute, chrono::Utc);
    (next_utc - chrono::Utc::now())
        .to_std()
        .unwrap_or_else(|_| Duration::from_secs(1))
}
