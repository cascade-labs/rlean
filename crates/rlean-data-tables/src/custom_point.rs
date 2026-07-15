use arrow_schema::{DataType, Field, Schema};
use rlean_core::{DateTime, TimeSpan};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use crate::{decimal_type, FieldDescription, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomDataPoint {
    pub time: DateTime,
    pub end_time: DateTime,
    pub value: Decimal,
    /// Provider-defined venue/series origin. This is part of row identity and
    /// is independent of the provider and feed routing columns.
    #[serde(default)]
    pub venue: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    pub fields: Arc<HashMap<String, serde_json::Value>>,
}

impl CustomDataPoint {
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
            venue: None,
            symbol: None,
            fields: Arc::new(fields),
        }
    }

    pub fn daily_eod(
        time: DateTime,
        value: Decimal,
        fields: HashMap<String, serde_json::Value>,
    ) -> Self {
        Self::new(time, time + TimeSpan::from_days(1), value, fields)
    }

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

    pub fn with_symbol(mut self, symbol: Option<String>) -> Self {
        self.symbol = symbol.map(|value| value.trim().to_ascii_uppercase());
        self
    }

    pub fn with_venue(mut self, venue: impl Into<String>) -> Self {
        self.venue = Some(venue.into());
        self
    }

    pub fn time_date(&self) -> chrono::NaiveDate {
        self.time.date_utc()
    }

    pub fn empty(time: DateTime, end_time: DateTime, value: Decimal) -> Self {
        Self::new(time, end_time, value, HashMap::new())
    }
}

impl TableContract for CustomDataPoint {
    const TABLE_NAME: &'static str = "custom_points";
    const DESCRIPTION: &'static str =
        "Provider-defined time series and events exposed to strategies as custom data.";
    const FIELD_DESCRIPTIONS: &'static [FieldDescription] = &[
        FieldDescription::new("time_ns", "Event or period start, Unix epoch nanoseconds (UTC)."),
        FieldDescription::new(
            "end_time_ns",
            "Availability frontier, Unix epoch nanoseconds (UTC); the point must not reach the algorithm before this time.",
        ),
        FieldDescription::new("value", "Primary numeric value used by BaseData.Value."),
        FieldDescription::new(
            "fields_json",
            "JSON object containing provider-specific fields; preserve original names and value types.",
        ),
        FieldDescription::new(
            "venue",
            "Provider-defined venue or series origin; distinguishes otherwise identical observations.",
        ),
        FieldDescription::new(
            "symbol_sid",
            "LEAN security identifier when the point is bound to a canonical Symbol.",
        ),
        FieldDescription::new(
            "symbol_value",
            "Display ticker or symbol value when the point is symbol-bound.",
        ),
        FieldDescription::new(
            "provider",
            "Stable lowercase integration name, such as fred or unusual_whales.",
        ),
        FieldDescription::new(
            "feed",
            "Stable lowercase dataset or series name within the provider.",
        ),
        FieldDescription::new(
            "resolution",
            "Derived cadence: tick, second, minute, hour, daily, or custom.",
        ),
        FieldDescription::new(
            "day",
            "UTC date of end_time_ns; used for pruning by the point's availability date.",
        ),
    ];
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("provider"),
        PartitionField::identity("resolution"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("time_ns", DataType::Int64, false),
            Field::new("end_time_ns", DataType::Int64, false),
            Field::new("value", decimal_type(), false),
            Field::new("fields_json", DataType::Utf8, false),
            // Nullable until the existing prototype data is backfilled.
            Field::new("venue", DataType::Utf8, true),
            Field::new("symbol_sid", DataType::Int64, true),
            Field::new("symbol_value", DataType::Utf8, true),
            Field::new("provider", DataType::Utf8, false),
            Field::new("feed", DataType::Utf8, false),
            Field::new("resolution", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
