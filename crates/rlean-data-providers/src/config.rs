/// Per-provider runtime configuration.
///
/// Passed to `DataProviderRegistry::build_*` factory functions and to the
/// `rlean` CLI `providers` module.  Fields that a particular provider does not
/// need are simply ignored.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// Root directory for the local Parquet data store.
    pub data_root: std::path::PathBuf,

    /// API key — required for `polygon`, optional for `thetadata`.
    pub api_key: Option<String>,

    /// Maximum requests per second to the remote API.
    /// `0.0` means unlimited (or provider default).
    pub requests_per_second: f64,

    /// Maximum concurrent in-flight requests.
    /// `0` means use the provider default.
    pub max_concurrent: usize,
}
