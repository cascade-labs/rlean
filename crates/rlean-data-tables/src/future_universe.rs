use arrow_schema::{DataType, Field, Schema};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FutureUniverseRow {
    /// C# `BaseData.Time`. `EndTime` is always the following day.
    pub date: NaiveDate,
    /// C# `Symbol.ID.Market`.
    pub market: String,
    /// C# `Symbol.SecurityType`.
    pub security_type: String,
    /// Canonical textual C# `Symbol.ID`. An integer hash is not a lossless SID.
    pub symbol_sid: String,
    /// C# `Symbol.Value`.
    pub symbol_value: String,
    /// Contract month serialized by `FutureUniverse.ToCsv`. This is distinct
    /// from the exchange-calculated expiration stored in the contract SID.
    pub contract_month: NaiveDate,
    /// C# `Symbol.ID.Date` (the calculated contract expiration).
    pub expiration: NaiveDate,
    /// C# `BaseChainUniverseData.Open`.
    pub open: Decimal,
    /// C# `BaseChainUniverseData.High`.
    pub high: Decimal,
    /// C# `BaseChainUniverseData.Low`.
    pub low: Decimal,
    /// C# `BaseChainUniverseData.Close` and inherited `Value`/`Price`.
    pub close: Decimal,
    /// C# `BaseChainUniverseData.Volume`.
    pub volume: Decimal,
    /// C# `BaseChainUniverseData.OpenInterest`; the CSV contract permits an
    /// absent value even though LEAN's convenience getter returns zero then.
    pub open_interest: Option<Decimal>,
}

impl TableContract for FutureUniverseRow {
    const TABLE_NAME: &'static str = "future_universe";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("market"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        let d = decimal_type();
        Arc::new(Schema::new(vec![
            Field::new("day", DataType::Date32, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("security_type", DataType::Utf8, false),
            Field::new("symbol_sid", DataType::Utf8, false),
            Field::new("symbol_value", DataType::Utf8, false),
            Field::new("contract_month", DataType::Date32, false),
            Field::new("expiration", DataType::Date32, false),
            Field::new("open", d.clone(), false),
            Field::new("high", d.clone(), false),
            Field::new("low", d.clone(), false),
            Field::new("close", d.clone(), false),
            Field::new("volume", d.clone(), false),
            Field::new("open_interest", d, true),
        ]))
    }
}
