use arrow_schema::{DataType, Field, Schema};
use rlean_core::DateTime;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, PartitionField, TableContract};

/// Provider-neutral annualized risk-free interest rate effective at `time`.
///
/// This mirrors LEAN's `IRiskFreeInterestRateModel`: consumers ask for the
/// latest rate effective on a date. `venue` records provenance (for example
/// `fred`) without leaking a vendor-specific series into engine APIs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskFreeInterestRate {
    pub time: DateTime,
    pub annual_rate: Decimal,
    #[serde(default)]
    pub venue: Option<String>,
}

impl RiskFreeInterestRate {
    pub fn new(time: DateTime, annual_rate: Decimal) -> Self {
        Self {
            time,
            annual_rate,
            venue: None,
        }
    }

    pub fn with_venue(mut self, venue: impl Into<String>) -> Self {
        self.venue = Some(venue.into());
        self
    }
}

impl TableContract for RiskFreeInterestRate {
    const TABLE_NAME: &'static str = "risk_free_interest_rates";
    const PARTITION_FIELDS: &'static [PartitionField] = &[PartitionField::month("day")];

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("time_ns", DataType::Int64, false),
            Field::new("annual_rate", decimal_type(), false),
            Field::new("venue", DataType::Utf8, true),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
