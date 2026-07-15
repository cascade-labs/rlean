use arrow_schema::{DataType, Field, Schema};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactorFileEntry {
    pub date: NaiveDate,
    pub price_factor: Decimal,
    pub split_factor: Decimal,
    pub reference_price: Decimal,
}

impl TableContract for FactorFileEntry {
    const TABLE_NAME: &'static str = "factor_files";
    const PARTITION_FIELDS: &'static [PartitionField] = &[PartitionField::identity("market")];
    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("date_ns", DataType::Int64, false),
            Field::new("price_factor", decimal_type(), false),
            Field::new("split_factor", decimal_type(), false),
            Field::new("reference_price", decimal_type(), false),
            Field::new("market", DataType::Utf8, false),
            Field::new("ticker", DataType::Utf8, false),
        ]))
    }
}

impl FactorFileEntry {
    /// Matches LEAN's derived `CorporateFactorRow.PriceScaleFactor` property.
    pub fn price_scale_factor(&self) -> Decimal {
        self.price_factor * self.split_factor
    }
}

impl FactorFileEntry {
    pub fn date_ns(&self) -> i64 {
        self.date
            .and_hms_opt(0, 0, 0)
            .and_then(|value| value.and_utc().timestamp_nanos_opt())
            .unwrap_or_default()
    }
}
