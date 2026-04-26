use chrono::NaiveDate;
use lean_data::custom::{
    CustomDataConfig, CustomDataPoint, CustomDataQuery, CustomDataSource, CustomParquetSource,
};
use std::sync::Arc;

/// Trait implemented by custom data source plugins.
///
/// Supports two custom data modes:
/// - Native parquet via `get_parquet_source()` and `is_parquet_native()`.
/// - Text sources via `get_source()` + `reader()`.
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

    /// Return parquet files for the given ticker/date/query when this provider
    /// can expose native parquet. The runner applies generic projection and
    /// predicates, then materializes `CustomDataPoint`s.
    fn get_parquet_source(
        &self,
        _ticker: &str,
        _date: NaiveDate,
        _config: &CustomDataConfig,
        _query: &CustomDataQuery,
    ) -> Option<CustomParquetSource> {
        None
    }

    /// Returns `true` for providers whose canonical storage is native parquet.
    /// The runner will not call `get_source()`/`reader()` for these providers;
    /// a missing parquet source means no data for that request.
    fn is_parquet_native(&self) -> bool {
        false
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

    /// Whether the data ticker requires symbol mapping (ticker rename history).
    ///
    /// Almost always `false` for alternative data; set to `true` only for
    /// sources that track equity corporate actions (e.g. equity fundamental data).
    fn requires_mapping(&self) -> bool {
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
}

/// Type-erased `Arc` wrapper — cloneable for use across threads and `RunConfig`.
pub type ArcCustomDataSource = Arc<dyn ICustomDataSource>;
