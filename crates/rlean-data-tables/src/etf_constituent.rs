use arrow_schema::{DataType, Field, Schema};
use chrono::NaiveDateTime;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EtfConstituentRow {
    pub time: NaiveDateTime,
    pub end_time: NaiveDateTime,
    /// C# `TimeSpan.Ticks` (one tick is 100 nanoseconds).
    pub period_ticks: i64,
    pub market: String,
    pub etf_sid: i64,
    pub etf_value: String,
    pub constituent_sid: i64,
    pub constituent_value: String,
    pub last_update: Option<NaiveDateTime>,
    pub weight: Option<Decimal>,
    pub shares_held: Option<Decimal>,
    pub market_value: Option<Decimal>,
}

impl TableContract for EtfConstituentRow {
    const TABLE_NAME: &'static str = "etf_constituents";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("market"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("time_ns", DataType::Int64, false),
            Field::new("end_time_ns", DataType::Int64, false),
            Field::new("period_ticks", DataType::Int64, false),
            Field::new("day", DataType::Date32, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("etf_sid", DataType::Int64, false),
            Field::new("etf_value", DataType::Utf8, false),
            Field::new("constituent_sid", DataType::Int64, false),
            Field::new("constituent_value", DataType::Utf8, false),
            Field::new("last_update_ns", DataType::Int64, true),
            Field::new("weight", decimal_type(), true),
            Field::new("shares_held", decimal_type(), true),
            Field::new("market_value", decimal_type(), true),
        ]))
    }
}
