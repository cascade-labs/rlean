use arrow_schema::{DataType, Field, Schema};
use rlean_core::{DateTime, Price, Symbol};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, BaseData, BaseDataType, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarginInterestRate {
    pub symbol: Symbol,
    #[serde(default)]
    pub venue: Option<String>,
    pub time: DateTime,
    pub interest_rate: Price,
}

impl MarginInterestRate {
    pub fn new(symbol: Symbol, time: DateTime, interest_rate: Price) -> Self {
        Self {
            symbol,
            venue: None,
            time,
            interest_rate,
        }
    }

    pub fn with_venue(mut self, venue: impl Into<String>) -> Self {
        self.venue = Some(venue.into());
        self
    }
}

impl BaseData for MarginInterestRate {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::MarginInterestRate
    }
    fn symbol(&self) -> &Symbol {
        &self.symbol
    }
    fn venue(&self) -> Option<&str> {
        self.venue.as_deref()
    }
    fn time(&self) -> DateTime {
        self.time
    }
    fn end_time(&self) -> DateTime {
        self.time
    }
    fn price(&self) -> Price {
        self.interest_rate
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}

impl TableContract for MarginInterestRate {
    const TABLE_NAME: &'static str = "margin_interest";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("security_type"),
        PartitionField::identity("market"),
        PartitionField::identity("resolution"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("time_ns", DataType::Int64, false),
            Field::new("symbol_sid", DataType::Int64, false),
            Field::new("symbol_value", DataType::Utf8, false),
            Field::new("venue", DataType::Utf8, true),
            Field::new("interest_rate", decimal_type(), false),
            Field::new("security_type", DataType::Utf8, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("resolution", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
