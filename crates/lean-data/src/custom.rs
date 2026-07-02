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
    ///
    /// The parquet reader also recognizes comma-separated `not_null` and
    /// `required_columns` values here as opt-in non-null row filters.
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
    Parquet,
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
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Native Parquet custom-data source returned directly by plugins.
///
/// This avoids forcing parquet-native providers to pretend each local file or
/// fetched object is a text/HTTP source. The engine owns decoding into
/// `CustomDataPoint`s.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomParquetSource {
    pub paths: Vec<String>,
    #[serde(default)]
    pub buffers: Vec<Vec<u8>>,
    pub time_column: Option<String>,
    pub time_format: Option<String>,
    pub time_zone: Option<String>,
    pub end_time_column: Option<String>,
    pub end_time_offset_nanos: Option<i64>,
    pub symbol_column: Option<String>,
    pub value_column: Option<String>,
    pub value_columns: Vec<String>,
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
