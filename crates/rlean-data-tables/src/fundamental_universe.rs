use arrow_schema::{DataType, Field, Schema};
use chrono::{NaiveDate, NaiveDateTime};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FundamentalUniverseRow {
    /// Session date on which this point is available to the selector.
    pub date: NaiveDate,
    pub time: NaiveDateTime,
    pub end_time: NaiveDateTime,
    pub market: String,
    pub symbol_sid: i64,
    pub symbol_value: String,
    pub volume: Decimal,
    /// Closing price multiplied by volume for the source session.
    pub dollar_volume: Decimal,
    pub market_cap: Decimal,
}

impl TableContract for FundamentalUniverseRow {
    const TABLE_NAME: &'static str = "fundamental_universe";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("market"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("day", DataType::Date32, false),
            Field::new("time_ns", DataType::Int64, false),
            Field::new("end_time_ns", DataType::Int64, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("symbol_sid", DataType::Int64, false),
            Field::new("symbol_value", DataType::Utf8, false),
            Field::new("volume", decimal_type(), false),
            Field::new("dollar_volume", decimal_type(), false),
            Field::new("market_cap", decimal_type(), false),
        ]))
    }
}
