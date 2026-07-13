use chrono::NaiveDate;
use rlean_core::{DateTime, Resolution, TimeSpan};
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
/// Mirrors LEAN C#'s `BaseData`, which carries exactly two time fields:
///
/// - [`time`](Self::time) — `BaseData.Time`, the start of the period the point
///   covers (a full UTC timestamp, not a bare date).
/// - [`end_time`](Self::end_time) — `BaseData.EndTime`, the end of the period.
///   This is the *emission gate*: LEAN sets `EmitTimeUtc = ConvertToUtc(EndTime)`
///   and the synchronizer only surfaces a point once the frontier reaches it,
///   which is what "ensures no look-ahead bias." It is never null in LEAN, so it
///   is a required field here — a point that could not be gated on a real
///   availability time cannot be placed on the timeline at all.
///
/// Both fields are always populated. Construct points through [`new`](Self::new)
/// (both times explicit), [`daily_eod`](Self::daily_eod) (the canonical daily
/// idiom: `end_time = time + 1 day`), or [`with_lean_defaulting`](Self::with_lean_defaulting)
/// (applies LEAN's `Time`/`EndTime` defaulting when only one of the two is known).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomDataPoint {
    /// Start of the period this point covers, in UTC. Mirrors LEAN `BaseData.Time`.
    pub time: DateTime,
    /// UTC end-of-period / emission gate. Mirrors LEAN `BaseData.EndTime`.
    /// Always populated — a point is never visible before this instant.
    pub end_time: DateTime,
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
    /// Construct a point with both `time` (period start) and `end_time` (the
    /// emission gate) explicit. Callers that only know one of the two should use
    /// [`with_lean_defaulting`](Self::with_lean_defaulting) or
    /// [`daily_eod`](Self::daily_eod) instead so LEAN's defaulting is applied.
    pub fn new(
        time: DateTime,
        end_time: DateTime,
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

    /// The canonical LEAN daily EOD idiom (`CustomDataBitcoinAlgorithm.py`):
    /// a point stamped at period start `time` with `end_time = time + 1 day`.
    /// Use for daily/EOD feeds (FRED, CBOE VIX, GDPNow) where the underlying
    /// data has no intraday timestamp.
    pub fn daily_eod(
        time: DateTime,
        value: Decimal,
        fields: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::new(time, time + TimeSpan::from_days(1), value, fields)
    }

    /// Apply LEAN's `Time`/`EndTime` defaulting rules (`BaseData.cs`,
    /// `PythonData.cs`) to whatever the provider supplied:
    ///
    /// - both set → used as-is;
    /// - only `time` set → `end_time = time` (instantaneous point);
    /// - only `end_time` set → `time = end_time`;
    /// - neither set → not representable — returns `None`, because a point with
    ///   no availability time cannot be safely placed on the timeline (issue #31:
    ///   never guess midnight).
    pub fn with_lean_defaulting(
        time: Option<DateTime>,
        end_time: Option<DateTime>,
        value: Decimal,
        fields: HashMap<String, serde_json::Value>,
    ) -> Option<Self> {
        let (time, end_time) = match (time, end_time) {
            (Some(time), Some(end_time)) => (time, end_time),
            (Some(time), None) => (time, time),
            (None, Some(end_time)) => (end_time, end_time),
            (None, None) => return None,
        };
        Some(Self::new(time, end_time, value, fields))
    }

    /// Builder that sets the canonical underlying symbol (uppercased).
    pub fn with_symbol(mut self, symbol: Option<String>) -> Self {
        self.symbol = symbol.map(|value| value.trim().to_ascii_uppercase());
        self
    }

    /// UTC calendar date of the period start (`time`). Convenience for callers
    /// that still reason in whole days (partition math, day-window filters).
    pub fn time_date(&self) -> NaiveDate {
        self.time.date_utc()
    }

    pub fn empty(time: DateTime, end_time: DateTime, value: Decimal) -> Self {
        Self::new(time, end_time, value, HashMap::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rust_decimal_macros::dec;

    fn point_with_symbol(symbol: Option<&str>) -> CustomDataPoint {
        let time = DateTime::from(
            NaiveDate::from_ymd_opt(2026, 7, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
                .and_utc(),
        );
        CustomDataPoint::daily_eod(time, dec!(1), HashMap::new())
            .with_symbol(symbol.map(str::to_string))
    }

    #[test]
    fn matches_point_filters_on_symbol_case_insensitively() {
        let query = CustomDataQuery {
            symbols: Some(vec!["nvda".to_string(), "META".to_string()]),
            ..Default::default()
        };

        assert!(query.matches_point(&point_with_symbol(Some("NVDA"))));
        assert!(query.matches_point(&point_with_symbol(Some("meta"))));
        assert!(!query.matches_point(&point_with_symbol(Some("SPY"))));
    }

    #[test]
    fn matches_point_drops_symbol_less_point_when_filter_active() {
        let query = CustomDataQuery {
            symbols: Some(vec!["NVDA".to_string()]),
            ..Default::default()
        };
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

    fn dt(ns: i64) -> DateTime {
        rlean_core::NanosecondTimestamp(ns)
    }

    #[test]
    fn daily_eod_sets_end_time_one_day_after_time() {
        let time = dt(1_704_067_200_000_000_000); // 2024-01-01 00:00:00 UTC
        let point = CustomDataPoint::daily_eod(time, dec!(1), HashMap::new());
        assert_eq!(point.time, time);
        assert_eq!(point.end_time, time + TimeSpan::from_days(1));
        assert_eq!(point.end_time.0 - point.time.0, 86_400 * 1_000_000_000);
    }

    #[test]
    fn lean_defaulting_only_time_sets_end_time_equal() {
        let time = dt(1_704_067_200_000_000_000);
        let point =
            CustomDataPoint::with_lean_defaulting(Some(time), None, dec!(1), HashMap::new())
                .unwrap();
        assert_eq!(point.time, time);
        assert_eq!(point.end_time, time);
    }

    #[test]
    fn lean_defaulting_only_end_time_sets_time_equal() {
        let end = dt(1_704_067_200_000_000_000);
        let point = CustomDataPoint::with_lean_defaulting(None, Some(end), dec!(1), HashMap::new())
            .unwrap();
        assert_eq!(point.time, end);
        assert_eq!(point.end_time, end);
    }

    #[test]
    fn lean_defaulting_neither_time_is_unrepresentable() {
        // Issue #31: a point with no availability time must not be placed on the
        // timeline (never guess midnight). Defaulting refuses it.
        assert!(
            CustomDataPoint::with_lean_defaulting(None, None, dec!(1), HashMap::new()).is_none()
        );
    }
}
