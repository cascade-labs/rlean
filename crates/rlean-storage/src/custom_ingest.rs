use std::collections::HashMap;

use arrow_array::{
    Array, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, LargeStringArray,
    RecordBatch, StringArray,
};
use arrow_schema::DataType;
use chrono::{NaiveDate, Utc};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rlean_core::{DateTime, TimeSpan};
use rlean_data::CustomDataPoint;
use rust_decimal::{prelude::FromPrimitive, Decimal};

pub fn provider_parquet_bytes_to_custom_points(
    bytes: &[u8],
    source_date: NaiveDate,
    source_uri: &str,
    value_columns: &[&str],
    symbol_column: Option<&str>,
) -> anyhow::Result<Vec<CustomDataPoint>> {
    let reader =
        ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes))?.build()?;
    let mut points = Vec::new();
    for batch in reader {
        append_batch_custom_points(
            &batch?,
            source_date,
            source_uri,
            value_columns,
            symbol_column,
            &mut points,
        );
    }
    Ok(points)
}

fn append_batch_custom_points(
    batch: &RecordBatch,
    source_date: NaiveDate,
    source_uri: &str,
    value_columns: &[&str],
    symbol_column: Option<&str>,
    out: &mut Vec<CustomDataPoint>,
) {
    for row in 0..batch.num_rows() {
        let fields = row_fields(batch, row);
        // Provider parquet feeds routed through this decoder are intraday event
        // feeds (TradeAlert sweeps/snapshot, Unusual Whales flow_alerts): each
        // row is an instantaneous event, so LEAN `Time` == `EndTime` == the
        // event's own timestamp. `point_event_time` derives that instant from
        // the row's timestamp fields (falling back to file/eastern-close time).
        let event_time = point_event_time(source_date, source_uri, &fields);
        let symbol = symbol_column.and_then(|column| point_symbol(&fields, column));
        out.push(
            CustomDataPoint::new(
                event_time,
                event_time,
                point_value(&fields, value_columns),
                fields,
            )
            .with_symbol(symbol),
        );
    }
}

/// Extract the canonical underlying symbol from a declared column. Returns
/// `None` when the column is missing, non-string, or trims to empty.
fn point_symbol(fields: &HashMap<String, serde_json::Value>, column: &str) -> Option<String> {
    let raw = fields.get(column).and_then(serde_json::Value::as_str)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_uppercase())
    }
}

fn row_fields(batch: &RecordBatch, row: usize) -> HashMap<String, serde_json::Value> {
    let mut fields = HashMap::new();
    for (idx, field) in batch.schema().fields().iter().enumerate() {
        fields.insert(
            field.name().clone(),
            arrow_cell_to_json(batch.column(idx).as_ref(), row),
        );
    }
    fields
}

fn arrow_cell_to_json(array: &dyn Array, row: usize) -> serde_json::Value {
    if array.is_null(row) {
        return serde_json::Value::Null;
    }
    match array.data_type() {
        DataType::Utf8 => serde_json::Value::String(
            array
                .as_any()
                .downcast_ref::<StringArray>()
                .expect("Utf8 array")
                .value(row)
                .to_string(),
        ),
        DataType::LargeUtf8 => serde_json::Value::String(
            array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .expect("LargeUtf8 array")
                .value(row)
                .to_string(),
        ),
        DataType::Float64 => serde_json::Value::from(
            array
                .as_any()
                .downcast_ref::<Float64Array>()
                .expect("Float64 array")
                .value(row),
        ),
        DataType::Float32 => serde_json::Value::from(
            array
                .as_any()
                .downcast_ref::<Float32Array>()
                .expect("Float32 array")
                .value(row) as f64,
        ),
        DataType::Int64 => serde_json::Value::from(
            array
                .as_any()
                .downcast_ref::<Int64Array>()
                .expect("Int64 array")
                .value(row),
        ),
        DataType::Int32 => serde_json::Value::from(
            array
                .as_any()
                .downcast_ref::<Int32Array>()
                .expect("Int32 array")
                .value(row),
        ),
        DataType::Boolean => serde_json::Value::from(
            array
                .as_any()
                .downcast_ref::<BooleanArray>()
                .expect("Boolean array")
                .value(row),
        ),
        _ => serde_json::Value::String(format!("{:?}", array.slice(row, 1))),
    }
}

fn point_value(fields: &HashMap<String, serde_json::Value>, value_columns: &[&str]) -> Decimal {
    for key in value_columns {
        if let Some(value) = fields.get(*key).and_then(serde_json::Value::as_f64) {
            if let Some(decimal) = Decimal::from_f64(value) {
                return decimal;
            }
        }
    }
    Decimal::ZERO
}

/// Derive the instantaneous event time (LEAN `Time` == `EndTime`) for an
/// intraday custom-data row, from its own timestamp fields. Preference order:
/// explicit epoch `end_time`/`start_time` columns, then textual event-time
/// columns (`time`/`bar_time`/`datetime`), then the file's `HHMM` name, then the
/// source date at the US-Eastern market close. Used both when decoding provider
/// parquet at ingest and when backfilling `time_ns`/`end_time_ns` during the
/// #81 in-place migration of intraday feeds.
pub fn point_event_time(
    source_date: NaiveDate,
    source_uri: &str,
    fields: &HashMap<String, serde_json::Value>,
) -> DateTime {
    if let Some(end_time) = fields
        .get("end_time")
        .and_then(json_timestamp)
        .or_else(|| fields.get("start_time").and_then(json_timestamp))
    {
        return end_time;
    }

    for key in ["time", "bar_time", "datetime"] {
        let Some(text) = fields.get(key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        if let Some(end_time) = parse_tradealert_timestamp(text, source_date) {
            return end_time;
        }
    }

    if let Some(time) = fields
        .get("time")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_hhmm)
    {
        return eastern_market_time(source_date, time);
    }

    if let Some(time) = file_hhmm(source_uri).and_then(parse_hhmm) {
        return eastern_market_time(source_date, time);
    }

    eastern_market_time(
        source_date,
        chrono::NaiveTime::from_hms_opt(16, 0, 0).expect("valid custom data timestamp"),
    )
}

fn json_timestamp(value: &serde_json::Value) -> Option<DateTime> {
    let raw = value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))?;
    Some(DateTime::from(rlean_core::NanosecondTimestamp(
        epoch_value_to_ns(raw),
    )))
}

/// Interprets an epoch integer of unknown unit (seconds, milliseconds,
/// microseconds, or nanoseconds) as nanoseconds-since-epoch, using magnitude
/// to disambiguate.
///
/// Real-world epoch values for any calendar date within a few centuries of
/// now cluster tightly by unit — seconds ~1e9-1e10, milliseconds ~1e12-1e13,
/// microseconds ~1e15-1e16, nanoseconds ~1e18-1e19 — with roughly three
/// orders of magnitude of empty space between adjacent clusters. Bucket
/// boundaries placed in those gaps classify unambiguously without the
/// producer having to declare a unit alongside the value. (A naive "is this
/// already ns" check at a single threshold like 1e15 misclassifies both
/// second-epoch and microsecond-epoch inputs — see the regression this
/// replaces.)
pub fn epoch_value_to_ns(raw: i64) -> i64 {
    match raw.unsigned_abs() {
        magnitude if magnitude < 100_000_000_000 => raw.saturating_mul(1_000_000_000), // seconds
        magnitude if magnitude < 100_000_000_000_000 => raw.saturating_mul(1_000_000), // millis
        magnitude if magnitude < 100_000_000_000_000_000 => raw.saturating_mul(1_000), // micros
        _ => raw, // already nanoseconds
    }
}

fn eastern_market_time(date: NaiveDate, time: chrono::NaiveTime) -> DateTime {
    use chrono::TimeZone as _;
    use chrono_tz::America::New_York;
    let local = New_York
        .from_local_datetime(&date.and_time(time))
        .single()
        .unwrap_or_else(|| New_York.from_utc_datetime(&date.and_time(time)));
    DateTime::from(local.with_timezone(&Utc)) + TimeSpan::ZERO
}

/// Parse TradeAlert sweep/snapshot timestamps. Values are exchange-local (US/Eastern).
pub fn parse_tradealert_timestamp(text: &str, source_date: NaiveDate) -> Option<DateTime> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S") {
        return Some(eastern_market_time(time.date(), time.time()));
    }
    if let Ok(time) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f") {
        return Some(eastern_market_time(time.date(), time.time()));
    }
    // TradeAlert occasionally uses "YYYY-MM-DD HH:MM:SS:mmm" (colon before millis).
    if let Some((base, _millis)) = text.rsplit_once(':') {
        if base.len() >= 19 {
            if let Ok(time) = chrono::NaiveDateTime::parse_from_str(base, "%Y-%m-%d %H:%M:%S") {
                return Some(eastern_market_time(time.date(), time.time()));
            }
        }
    }
    if let Some(time) = parse_hhmm(text) {
        return Some(eastern_market_time(source_date, time));
    }
    None
}

fn parse_hhmm(value: &str) -> Option<chrono::NaiveTime> {
    let digits = value.trim().replace(':', "");
    if digits.len() < 4 {
        return None;
    }
    let hour = digits[0..2].parse().ok()?;
    let minute = digits[2..4].parse().ok()?;
    chrono::NaiveTime::from_hms_opt(hour, minute, 0)
}

fn file_hhmm(uri: &str) -> Option<&str> {
    uri.rsplit('/').next()?.strip_suffix(".parquet")
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::LargeStringArray;
    use arrow_schema::{Field, Schema};
    use std::sync::Arc;

    #[test]
    fn epoch_value_to_ns_classifies_seconds_millis_micros_nanos() {
        // 2024-01-02 00:00:00 UTC, expressed in each candidate unit.
        let seconds = 1_704_153_600_i64;
        let millis = seconds * 1_000;
        let micros = seconds * 1_000_000;
        let nanos = seconds * 1_000_000_000;

        assert_eq!(epoch_value_to_ns(seconds), nanos, "seconds epoch");
        assert_eq!(epoch_value_to_ns(millis), nanos, "milliseconds epoch");
        assert_eq!(epoch_value_to_ns(micros), nanos, "microseconds epoch");
        assert_eq!(epoch_value_to_ns(nanos), nanos, "nanoseconds epoch");
    }

    #[test]
    fn prefers_event_time_over_current_time_metadata() {
        let mut fields = HashMap::new();
        fields.insert(
            "current_time".into(),
            serde_json::Value::String("2026-06-14 18:47:31".into()),
        );
        fields.insert(
            "time".into(),
            serde_json::Value::String("2026-04-01 09:30:07:650".into()),
        );
        let source_date = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let end_time = point_event_time(source_date, "0931.parquet", &fields);
        assert_eq!(end_time.date_utc(), source_date);
    }

    #[test]
    fn prefers_end_time_millis_column() {
        let mut fields = HashMap::new();
        fields.insert(
            "current_time".into(),
            serde_json::Value::String("2026-06-14 18:47:31".into()),
        );
        fields.insert(
            "end_time".into(),
            serde_json::Value::from(1_775_050_200_971_i64),
        );
        let source_date = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let end_time = point_event_time(source_date, "0931.parquet", &fields);
        assert_eq!(end_time.date_utc(), source_date);
    }

    #[test]
    fn ignores_current_time_for_event_time_fallback() {
        let mut fields = HashMap::new();
        fields.insert(
            "current_time".into(),
            serde_json::Value::String("2026-06-14 18:47:31".into()),
        );
        let source_date = NaiveDate::from_ymd_opt(2026, 4, 1).unwrap();
        let end_time = point_event_time(source_date, "0931.parquet", &fields);
        assert_eq!(end_time.date_utc(), source_date);
    }

    #[test]
    fn large_utf8_cells_decode_as_scalar_strings() {
        let schema = Arc::new(Schema::new(vec![Field::new(
            "usymbol",
            DataType::LargeUtf8,
            false,
        )]));
        let batch =
            RecordBatch::try_new(schema, vec![Arc::new(LargeStringArray::from(vec!["NVDA"]))])
                .unwrap();

        let fields = row_fields(&batch, 0);

        assert_eq!(
            fields.get("usymbol").and_then(serde_json::Value::as_str),
            Some("NVDA")
        );
    }

    #[test]
    fn provider_parquet_populates_symbol_from_declared_column() {
        use arrow_array::{Float64Array, Int64Array, StringArray};
        use parquet::arrow::ArrowWriter;

        let source_date = NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let end_ns = schema_date_ns(source_date);
        let schema = Arc::new(Schema::new(vec![
            Field::new("end_time", DataType::Int64, false),
            Field::new("value", DataType::Float64, false),
            Field::new("usymbol", DataType::Utf8, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![end_ns])),
                Arc::new(Float64Array::from(vec![7.0])),
                Arc::new(StringArray::from(vec![" spy "])),
            ],
        )
        .unwrap();
        let mut bytes = Vec::new();
        {
            let mut writer = ArrowWriter::try_new(&mut bytes, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
        }

        let points = provider_parquet_bytes_to_custom_points(
            &bytes,
            source_date,
            "1600.parquet",
            &["value"],
            Some("usymbol"),
        )
        .unwrap();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].symbol.as_deref(), Some("SPY"));

        // Without a declared symbol column, the point carries no symbol.
        let no_symbol = provider_parquet_bytes_to_custom_points(
            &bytes,
            source_date,
            "1600.parquet",
            &["value"],
            None,
        )
        .unwrap();
        assert_eq!(no_symbol[0].symbol, None);
    }

    fn schema_date_ns(date: NaiveDate) -> i64 {
        date.and_hms_opt(16, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap()
    }
}
