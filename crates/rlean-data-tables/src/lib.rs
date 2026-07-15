//! Canonical rlean data types and provider-neutral Arrow table contracts.
//!
//! Sidecars, the runtime engine, and persistence all use these same values.

pub mod base_data;
pub mod contract;
pub mod custom_point;
pub mod etf_constituent;
pub mod factor_file;
pub mod fundamental_universe;
pub mod future_universe;
pub mod map_file;
pub mod margin_interest;
pub mod market_quote_bar;
pub mod market_tick;
pub mod market_trade_bar;
pub mod option_universe;

pub use base_data::{BaseData, BaseDataType};
pub use contract::{
    decimal_type, FieldDescription, PartitionField, PartitionTransform, TableContract,
    TableDefinition, DECIMAL_PRECISION, DECIMAL_SCALE,
};
pub use custom_point::CustomDataPoint;
pub use etf_constituent::EtfConstituentRow;
pub use factor_file::FactorFileEntry;
pub use fundamental_universe::FundamentalUniverseRow;
pub use future_universe::FutureUniverseRow;
pub use map_file::{DataMappingMode, MapFileEntry};
pub use margin_interest::MarginInterestRate;
pub use market_quote_bar::{Bar, QuoteBar};
pub use market_tick::Tick;
pub use market_trade_bar::{TradeBar, TradeBarData};
pub use option_universe::OptionUniverseRow;

/// Every table rlean knows how to initialize. Adding a new row contract only
/// requires registering its type here; catalog-specific code remains generic.
pub fn table_definitions() -> Vec<TableDefinition> {
    vec![
        TradeBar::table_definition(),
        QuoteBar::table_definition(),
        Tick::table_definition(),
        MarginInterestRate::table_definition(),
        CustomDataPoint::table_definition(),
        OptionUniverseRow::table_definition(),
        FutureUniverseRow::table_definition(),
        FundamentalUniverseRow::table_definition(),
        EtfConstituentRow::table_definition(),
        FactorFileEntry::table_definition(),
        MapFileEntry::table_definition(),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn registry_names_are_unique_and_partitions_reference_schema_fields() {
        let definitions = table_definitions();
        let names = definitions
            .iter()
            .map(|table| table.name)
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), definitions.len());

        for table in definitions {
            for partition in table.partition_fields {
                assert!(
                    table.schema.field_with_name(partition.source).is_ok(),
                    "{}.{} is not a schema field",
                    table.name,
                    partition.source
                );
            }
        }
    }
}
