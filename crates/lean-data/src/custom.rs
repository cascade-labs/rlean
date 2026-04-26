use chrono::NaiveDate;
use lean_core::{DateTime, Resolution};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Generic query hints for custom data providers.
///
/// Providers may use these to push filtering/projection into their native
/// storage layer. The runner also uses them for parquet-capable providers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CustomDataQuery {
    /// Provider-neutral symbol filter. Providers define which column this maps
    /// to, but `usymbol` is the common convention for TradeAlert-style data.
    pub symbols: Option<Vec<String>>,
    /// Provider field projection. Providers should include any required time,
    /// value, and symbol columns even if omitted here.
    pub columns: Option<Vec<String>>,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    /// Inclusive lower timestamp bound in UTC.
    pub start_time: Option<DateTime>,
    /// Inclusive upper timestamp bound in UTC.
    pub end_time: Option<DateTime>,
    pub string_equals: HashMap<String, String>,
    pub string_in: HashMap<String, Vec<String>>,
    pub numeric_min: HashMap<String, f64>,
    pub numeric_max: HashMap<String, f64>,
    /// Provider-specific settings not covered by the generic fields.
    pub properties: HashMap<String, String>,
}

impl CustomDataQuery {
    pub fn merge(&self, overlay: &CustomDataQuery) -> CustomDataQuery {
        let mut merged = self.clone();
        if overlay.symbols.is_some() {
            merged.symbols = overlay.symbols.clone();
        }
        if overlay.columns.is_some() {
            merged.columns = overlay.columns.clone();
        }
        if overlay.start_date.is_some() {
            merged.start_date = overlay.start_date;
        }
        if overlay.end_date.is_some() {
            merged.end_date = overlay.end_date;
        }
        if overlay.start_time.is_some() {
            merged.start_time = overlay.start_time;
        }
        if overlay.end_time.is_some() {
            merged.end_time = overlay.end_time;
        }
        merged.string_equals.extend(
            overlay
                .string_equals
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        merged.string_in.extend(
            overlay
                .string_in
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        merged
            .numeric_min
            .extend(overlay.numeric_min.iter().map(|(k, v)| (k.clone(), *v)));
        merged
            .numeric_max
            .extend(overlay.numeric_max.iter().map(|(k, v)| (k.clone(), *v)));
        merged.properties.extend(
            overlay
                .properties
                .iter()
                .map(|(k, v)| (k.clone(), v.clone())),
        );
        merged
    }
}

/// A parquet-native source returned by custom data providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomParquetSource {
    pub paths: Vec<String>,
    /// Column used as the data timestamp/date. If absent, the runner uses the
    /// requested date for every row.
    pub time_column: Option<String>,
    /// Timestamp encoding for `time_column`.
    ///
    /// Native custom parquet supports one LEAN-compatible value:
    /// `timestamp`, an Arrow timestamp column. If `time_zone` is set, the
    /// timestamp is interpreted as local wall-clock time in that zone and
    /// converted to UTC.
    pub time_format: Option<String>,
    /// Time zone for provider-local Arrow timestamp columns. Omit for UTC.
    pub time_zone: Option<String>,
    /// Primary symbol column for generic `symbols` filtering.
    pub symbol_column: Option<String>,
    /// Primary numeric value column for `CustomDataPoint.value`.
    pub value_column: Option<String>,
}

/// Configuration for a custom data subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomDataConfig {
    pub ticker: String,
    /// Unique name matching the plugin registry entry (e.g. "fred", "cboe_vix").
    pub source_type: String,
    pub resolution: Resolution,
    /// Arbitrary string properties passed to the plugin (API keys, etc.).
    pub properties: HashMap<String, String>,
    pub query: CustomDataQuery,
}

/// Transport mechanism for fetching custom data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CustomDataTransport {
    LocalFile,
    Http,
}

/// Wire format of the fetched data.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CustomDataFormat {
    Csv,
    Json,
}

/// Describes where to fetch custom data for a given ticker + date.
///
/// Returned by `ICustomDataSource::get_source` — mirrors LEAN's
/// `BaseData.GetSource` return value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomDataSource {
    /// URL (HTTP) or file path (LocalFile).
    pub uri: String,
    pub transport: CustomDataTransport,
    pub format: CustomDataFormat,
}

/// A single data point returned by a custom data source.
///
/// Mirrors LEAN C#'s `BaseData` with `Time` + `Value` + extra fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomDataPoint {
    /// The date/time this point applies to (start of the period).
    pub time: NaiveDate,
    /// UTC emission/end time. Mirrors LEAN `BaseData.EndTime`.
    #[serde(default)]
    pub end_time: Option<DateTime>,
    /// Primary scalar value (equivalent to LEAN's `BaseData.Value`).
    pub value: Decimal,
    /// Additional named fields (e.g. open/high/low/close for VIX).
    pub fields: HashMap<String, serde_json::Value>,
}

/// Active custom data subscription for one ticker + source type.
#[derive(Debug, Clone)]
pub struct CustomDataSubscription {
    pub source_type: String,
    pub ticker: String,
    pub config: CustomDataConfig,
    pub dynamic_query: CustomDataQuery,
}
