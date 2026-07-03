use std::collections::HashMap;

use arrow_array::{
    Array, BooleanArray, Float32Array, Float64Array, Int32Array, Int64Array, RecordBatch,
    StringArray,
};
use arrow_schema::DataType;
use chrono::{NaiveDate, Utc};
use lean_core::{DateTime, TimeSpan};
use lean_data::CustomDataPoint;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use rust_decimal::{prelude::FromPrimitive, Decimal};

pub fn provider_parquet_bytes_to_custom_points(
    bytes: &[u8],
    source_date: NaiveDate,
    source_uri: &str,
    value_columns: &[&str],
) -> anyhow::Result<Vec<CustomDataPoint>> {
    let reader =
        ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes))?.build()?;
    let mut points = Vec::new();
    for batch in reader {
        append_batch_custom_points(&batch?, source_date, source_uri, value_columns, &mut points);
    }
    Ok(points)
}

fn append_batch_custom_points(
    batch: &RecordBatch,
    source_date: NaiveDate,
    source_uri: &str,
    value_columns: &[&str],
    out: &mut Vec<CustomDataPoint>,
) {
    for row in 0..batch.num_rows() {
        let fields = row_fields(batch, row);
        out.push(CustomDataPoint {
            time: source_date,
            end_time: Some(point_end_time(source_date, source_uri, &fields)),
            value: point_value(&fields, value_columns),
            fields,
        });
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

fn point_end_time(
    source_date: NaiveDate,
    source_uri: &str,
    fields: &HashMap<String, serde_json::Value>,
) -> DateTime {
    for key in ["current_time", "time", "bar_time", "datetime"] {
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
