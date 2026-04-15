use std::collections::HashMap;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use lean_core::Resolution;

/// Configuration for a custom data subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomDataConfig {
    pub ticker: String,
    /// Unique name matching the plugin registry entry (e.g. "fred", "cboe_vix").
    pub source_type: String,
    pub resolution: Resolution,
    /// Arbitrary string properties passed to the plugin (API keys, etc.).
    pub properties: HashMap<String, String>,
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
    JsonLines,
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
}
