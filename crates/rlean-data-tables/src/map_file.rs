use arrow_schema::{DataType, Field, Schema};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{PartitionField, TableContract};

/// Numeric values match `QuantConnect.DataMappingMode` exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum DataMappingMode {
    LastTradingDay = 0,
    FirstDayMonth = 1,
    OpenInterest = 2,
    OpenInterestAnnual = 3,
}

impl TableContract for MapFileEntry {
    const TABLE_NAME: &'static str = "map_files";
    const PARTITION_FIELDS: &'static [PartitionField] = &[PartitionField::identity("market")];
    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("date_ns", DataType::Int64, false),
            Field::new("mapped_symbol", DataType::Utf8, false),
            Field::new("primary_exchange_code", DataType::Utf8, false),
            Field::new("data_mapping_mode", DataType::Int32, true),
            Field::new("market", DataType::Utf8, false),
            Field::new("permtick", DataType::Utf8, false),
        ]))
    }
}

impl TryFrom<i32> for DataMappingMode {
    type Error = i32;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::LastTradingDay),
            1 => Ok(Self::FirstDayMonth),
            2 => Ok(Self::OpenInterest),
            3 => Ok(Self::OpenInterestAnnual),
            value => Err(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapFileEntry {
    pub date: NaiveDate,
    pub mapped_symbol: String,
    /// LEAN serializes `PrimaryExchange` by its exchange code in map files.
    pub primary_exchange_code: String,
    pub data_mapping_mode: Option<DataMappingMode>,
}

impl MapFileEntry {
    pub fn date_ns(&self) -> i64 {
        self.date
            .and_hms_opt(0, 0, 0)
            .and_then(|value| value.and_utc().timestamp_nanos_opt())
            .unwrap_or_default()
    }
}
