use chrono::NaiveDate;
use rlean_core::{DateTime, Resolution};
use rlean_data_tables::CustomDataPoint;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Generic query hints for custom data providers.
///
/// Providers may use these to push filtering/projection into their native
/// canonical data contract. The runner forwards them with subscriptions.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CustomDataQuery {
    /// Provider-neutral symbol filter matched against the canonical
    /// [`CustomDataPoint::symbol`] case-insensitively.
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
    /// Providers may also recognize comma-separated `not_null` and
    /// `required_columns` values here as opt-in non-null row filters.
    pub properties: HashMap<String, String>,
}

impl CustomDataQuery {
    /// Build the provider-neutral query represented by strategy `AddData`
    /// properties. Filter prefixes are part of the rlean data contract, so
    /// parsing them here keeps Rust and Python subscription paths identical.
    pub fn from_properties(properties: &HashMap<String, String>) -> Self {
        fn split_csv(value: &str) -> Vec<String> {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect()
        }

        let mut query = Self::default();
        if let Some(symbols) = properties.get("symbols") {
            query.symbols = Some(split_csv(symbols));
        }
        if let Some(columns) = properties.get("columns") {
            query.columns = Some(split_csv(columns));
        }
        for (key, value) in properties {
            if let Some(column) = key.strip_prefix("eq_") {
                query
                    .string_equals
                    .insert(column.to_string(), value.clone());
            } else if let Some(column) = key.strip_prefix("in_") {
                query.string_in.insert(column.to_string(), split_csv(value));
            } else if let Some(column) = key.strip_prefix("min_") {
                if let Ok(value) = value.parse::<f64>() {
                    query.numeric_min.insert(column.to_string(), value);
                }
            } else if let Some(column) = key.strip_prefix("max_") {
                if let Ok(value) = value.parse::<f64>() {
                    query.numeric_max.insert(column.to_string(), value);
                }
            }
        }
        query.properties = properties.clone();
        query
    }

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
    /// filters. Historical queries and live delivery
    /// use this so live custom data is filtered identically to backtests.
    ///
    /// Symbol matching is case-insensitive against [`CustomDataPoint::symbol`],
    /// which providers populate directly (live) or the engine copies from the
    /// declared `symbol_column`.
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
    /// Unique provider series identifier (e.g. "fred", "cboe_vix").
    pub source_type: String,
    pub resolution: Resolution,
    /// Arbitrary string properties passed to the selected provider.
    pub properties: HashMap<String, String>,
    pub query: CustomDataQuery,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rlean_core::TimeSpan;
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
    fn from_properties_maps_strategy_filter_prefixes() {
        let properties = HashMap::from([
            ("symbols".to_string(), "SPY, AAPL".to_string()),
            ("eq_side".to_string(), "ask".to_string()),
            ("in_type".to_string(), "call, put".to_string()),
            ("min_norm_mid_edge".to_string(), "500".to_string()),
            ("max_size".to_string(), "bad".to_string()),
        ]);

        let query = CustomDataQuery::from_properties(&properties);

        assert_eq!(
            query.symbols,
            Some(vec!["SPY".to_string(), "AAPL".to_string()])
        );
        assert_eq!(query.string_equals.get("side"), Some(&"ask".to_string()));
        assert_eq!(
            query.string_in.get("type"),
            Some(&vec!["call".to_string(), "put".to_string()])
        );
        assert_eq!(query.numeric_min.get("norm_mid_edge"), Some(&500.0));
        assert!(!query.numeric_max.contains_key("size"));
        assert_eq!(query.properties, properties);
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
        // when a symbol filter is active (mirrors historical provider queries).
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
