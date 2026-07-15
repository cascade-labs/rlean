use arrow_schema::{DataType, Field, Schema};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, PartitionField, TableContract};

/// One entry from LEAN's daily `OptionUniverse` collection.
///
/// `symbol_sid` and `underlying_sid` contain the canonical textual LEAN
/// `SecurityIdentifier`; an integer hash cannot losslessly represent a C# SID.
/// Option-specific identity and analytics are nullable because an
/// `OptionUniverse` collection also contains its non-option underlying row and
/// because futures-option universe files do not contain IV or Greeks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionUniverseRow {
    /// C# `BaseData.Time`. `EndTime` is always the following day.
    pub date: NaiveDate,
    /// C# `Symbol.ID.Market`.
    pub market: String,
    /// C# `Symbol.SecurityType` (`Option`, `IndexOption`, `FutureOption`, or the
    /// security type of the collection's underlying row).
    pub security_type: String,
    /// Canonical C# `Symbol.ID` string.
    pub symbol_sid: String,
    /// C# `Symbol.Value`.
    pub symbol_value: String,
    /// Canonical C# `Symbol.ID.Underlying` string; absent on the underlying row.
    pub underlying_sid: Option<String>,
    /// C# `Symbol.Underlying.Value`; absent on the underlying row.
    pub underlying_value: Option<String>,
    /// C# `Symbol.ID.Date`; absent on the underlying row.
    pub expiration: Option<NaiveDate>,
    /// C# `Symbol.ID.StrikePrice`; absent on the underlying row.
    pub strike: Option<Decimal>,
    /// C# `Symbol.ID.OptionRight`; absent on the underlying row.
    pub right: Option<String>,
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
    /// C# `OptionUniverse.ImpliedVolatility`; unavailable for futures options
    /// and the underlying row.
    pub implied_volatility: Option<Decimal>,
    /// C# `OptionUniverse.Greeks.Delta`.
    pub delta: Option<Decimal>,
    /// C# `OptionUniverse.Greeks.Gamma`.
    pub gamma: Option<Decimal>,
    /// C# `OptionUniverse.Greeks.Vega`.
    pub vega: Option<Decimal>,
    /// C# `OptionUniverse.Greeks.Theta` (annualized, not CSV `ThetaPerDay`).
    pub theta: Option<Decimal>,
    /// C# `OptionUniverse.Greeks.Rho`.
    pub rho: Option<Decimal>,
}

impl TableContract for OptionUniverseRow {
    const TABLE_NAME: &'static str = "option_universe";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("market"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        let d = decimal_type();
        Arc::new(Schema::new(vec![
            Field::new("date_ns", DataType::Int64, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("security_type", DataType::Utf8, false),
            Field::new("symbol_sid", DataType::Utf8, false),
            Field::new("symbol_value", DataType::Utf8, false),
            Field::new("underlying_sid", DataType::Utf8, true),
            Field::new("underlying_value", DataType::Utf8, true),
            Field::new("expiration_ns", DataType::Int64, true),
            Field::new("strike", d.clone(), true),
            Field::new("right", DataType::Utf8, true),
            Field::new("open", d.clone(), false),
            Field::new("high", d.clone(), false),
            Field::new("low", d.clone(), false),
            Field::new("close", d.clone(), false),
            Field::new("volume", d.clone(), false),
            Field::new("open_interest", d.clone(), true),
            Field::new("implied_volatility", d.clone(), true),
            Field::new("delta", d.clone(), true),
            Field::new("gamma", d.clone(), true),
            Field::new("vega", d.clone(), true),
            Field::new("theta", d.clone(), true),
            Field::new("rho", d, true),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
