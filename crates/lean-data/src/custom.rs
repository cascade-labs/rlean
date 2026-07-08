use chrono::NaiveDate;
use lean_core::{DateTime, Resolution};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

/// Generic query hints for custom data providers.
///
/// Providers may use these to push filtering/projection into their native
/// storage layer. The runner also uses them for parquet-capable providers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CustomDataQuery {
    /// Provider-neutral symbol filter. Matched against the canonical
    /// [`CustomDataPoint::symbol`], which providers populate by declaring a
    /// `symbol_column` on their [`CustomDataSource`] (the engine copies that
    /// column's value, uppercased, into the point). Matching is
    /// case-insensitive.
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

    /// Whether a decoded point satisfies this query's symbol, string, and numeric
    /// filters. Both the historical (Iceberg/parquet) reader and the live poller
    /// use this so live custom data is filtered identically to backtests.
    ///
    /// Symbol matching is case-insensitive against [`CustomDataPoint::symbol`],
    /// which providers populate directly (live) or the engine copies from the
    /// declared `symbol_column` (historical parquet).
    pub fn matches_point(&self, point: &CustomDataPoint) -> bool {
        if let Some(symbols) = &self.symbols {
            let Some(point_symbol) = point.symbol.as_deref() else {
                return false;
            };
            if !symbols
                .iter()
                .any(|symbol| symbol.eq_ignore_ascii_case(point_symbol))
            {
                return false;
            }
        }

        for (field, expected) in &self.string_equals {
            if point
                .fields
                .get(field)
                .and_then(|value| value.as_str())
                .map(|actual| actual == expected)
                != Some(true)
            {
                return false;
            }
        }

        for (field, expected_values) in &self.string_in {
            let Some(actual) = point.fields.get(field).and_then(|value| value.as_str()) else {
                return false;
            };
            if !expected_values.iter().any(|expected| expected == actual) {
                return false;
            }
        }

        for (field, min_value) in &self.numeric_min {
            if point
                .fields
                .get(field)
                .and_then(json_number)
                .map(|actual| actual >= *min_value)
                != Some(true)
            {
                return false;
            }
        }

        for (field, max_value) in &self.numeric_max {
            if point
                .fields
                .get(field)
                .and_then(json_number)
                .map(|actual| actual <= *max_value)
                != Some(true)
            {
                return false;
            }
        }

        true
    }
}

fn json_number(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<f64>().ok()))
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
    /// Name of the decoded column carrying the underlying symbol. When set, the
    /// engine copies that column's value (uppercased) into
    /// [`CustomDataPoint::symbol`] so `CustomDataQuery::symbols` can filter on
    /// it. `None` leaves the point's symbol unset.
    #[serde(default)]
    pub symbol_column: Option<String>,
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
    /// Canonical UPPERCASE underlying ticker this point pertains to, if any.
    ///
    /// Providers declare which decoded column carries it via
    /// [`CustomDataSource::symbol_column`]; the engine uppercases and stores it
    /// here. `CustomDataQuery::symbols` filtering matches against this field.
    #[serde(default)]
    pub symbol: Option<String>,
    /// Additional named fields (e.g. open/high/low/close for VIX).
    pub fields: Arc<HashMap<String, serde_json::Value>>,
}

impl CustomDataPoint {
    pub fn new(
        time: NaiveDate,
        end_time: Option<DateTime>,
        value: Decimal,
        fields: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self {
            time,
            end_time,
            value,
            symbol: None,
            fields: Arc::new(fields),
        }
    }

    /// Builder that sets the canonical underlying symbol (uppercased).
    pub fn with_symbol(mut self, symbol: Option<String>) -> Self {
        self.symbol = symbol.map(|value| value.trim().to_ascii_uppercase());
        self
    }

    pub fn empty(time: NaiveDate, end_time: Option<DateTime>, value: Decimal) -> Self {
        Self::new(time, end_time, value, HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal_macros::dec;

    fn point_with_symbol(symbol: Option<&str>) -> CustomDataPoint {
        CustomDataPoint::empty(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(), None, dec!(1))
            .with_symbol(symbol.map(str::to_string))
    }

    #[test]
    fn matches_point_filters_on_symbol_case_insensitively() {
        let mut query = CustomDataQuery::default();
        query.symbols = Some(vec!["nvda".to_string(), "META".to_string()]);

        assert!(query.matches_point(&point_with_symbol(Some("NVDA"))));
        assert!(query.matches_point(&point_with_symbol(Some("meta"))));
        assert!(!query.matches_point(&point_with_symbol(Some("SPY"))));
    }

    #[test]
    fn matches_point_drops_symbol_less_point_when_filter_active() {
        let mut query = CustomDataQuery::default();
        query.symbols = Some(vec!["NVDA".to_string()]);
        // Live providers must populate point.symbol; an unset symbol is dropped
        // when a symbol filter is active (mirrors the historical parquet path).
        assert!(!query.matches_point(&point_with_symbol(None)));
    }

    #[test]
    fn matches_point_accepts_any_symbol_without_filter() {
        let query = CustomDataQuery::default();
        assert!(query.matches_point(&point_with_symbol(Some("ANYTHING"))));
        assert!(query.matches_point(&point_with_symbol(None)));
    }
}
