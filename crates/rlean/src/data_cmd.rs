use anyhow::{bail, Result};
use arrow::datatypes::DataType;
use clap::{Args, Subcommand};
use rlean_data_tables::{table_definitions, PartitionTransform, TableDefinition};

#[derive(Args)]
pub(crate) struct DataArgs {
    #[command(subcommand)]
    pub(crate) command: DataCommand,
}

#[derive(Subcommand)]
pub(crate) enum DataCommand {
    /// List the canonical rlean data tables and their partition specs
    Tables,

    /// Show the canonical schema and partition spec for a table
    Schema {
        /// Table name, optionally qualified with a namespace
        table: String,
    },
}

pub(crate) async fn run_data(args: DataArgs) -> Result<()> {
    match args.command {
        DataCommand::Tables => {
            print!("{}", render_tables());
            Ok(())
        }
        DataCommand::Schema { table } => show_schema(&table),
    }
}

fn show_schema(requested: &str) -> Result<()> {
    let name = requested.rsplit('.').next().unwrap_or(requested);
    let definitions = table_definitions();
    let Some(table) = definitions.iter().find(|table| table.name == name) else {
        let available = definitions
            .iter()
            .map(|table| table.name)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("unknown rlean data table '{requested}'. Available tables: {available}");
    };
    print!("{}", render_schema(table));
    Ok(())
}

fn render_tables() -> String {
    let mut output = String::new();
    for table in table_definitions() {
        let partitions = table
            .partition_fields
            .iter()
            .map(render_partition)
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!("{}\t{}\n", table.name, partitions));
    }
    output
}

fn render_schema(table: &TableDefinition) -> String {
    let mut output = format!("Table: {}\n", table.name);
    if !table.description.is_empty() {
        output.push_str(&format!("Purpose: {}\n", table.description));
    }
    output.push_str("\nFields:\n");
    let name_width = table
        .schema
        .fields()
        .iter()
        .map(|field| field.name().len())
        .max()
        .unwrap_or(4)
        .max(4);
    output.push_str(&format!(
        "{:<name_width$}  {:<18}  {:<8}  {}\n",
        "NAME", "TYPE", "REQUIRED", "DESCRIPTION"
    ));
    for field in table.schema.fields() {
        let description = table
            .field_descriptions
            .iter()
            .find(|entry| entry.name == field.name())
            .map(|entry| entry.description)
            .unwrap_or("");
        output.push_str(&format!(
            "{:<name_width$}  {:<18}  {:<8}  {}\n",
            field.name(),
            contract_type_name(field.data_type()),
            if field.is_nullable() { "no" } else { "yes" },
            description
        ));
    }
    output.push_str("\nPartition spec:\n");
    for partition in table.partition_fields {
        output.push_str(&format!("- {}\n", render_partition(partition)));
    }
    output
}

fn contract_type_name(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "boolean".into(),
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::UInt8 => "int".into(),
        DataType::Int64 | DataType::UInt64 => "long".into(),
        DataType::Float32 => "float".into(),
        DataType::Float64 => "double".into(),
        DataType::Utf8 | DataType::LargeUtf8 => "string".into(),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Timestamp(_, _) => "timestamp_ns".into(),
        DataType::Decimal128(precision, scale) => format!("decimal({precision},{scale})"),
        other => format!("{other:?}"),
    }
}

fn render_partition(field: &rlean_data_tables::PartitionField) -> String {
    match field.transform {
        PartitionTransform::Identity => format!("identity({})", field.source),
        PartitionTransform::Month => format!("month({})", field.source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trade_bar_schema_is_rendered_from_the_contract() {
        let table = table_definitions()
            .into_iter()
            .find(|table| table.name == "market_trade_bars")
            .unwrap();
        let output = render_schema(&table);
        assert!(output.contains("open           decimal(38,18)"));
        assert!(output.contains("- month(day)"));
    }

    #[test]
    fn custom_point_schema_includes_implementor_guidance() {
        let table = table_definitions()
            .into_iter()
            .find(|table| table.name == "custom_points")
            .unwrap();
        let output = render_schema(&table);
        assert!(output.contains("Provider-defined time series and events"));
        assert!(output.contains("Availability frontier"));
        assert!(output.contains("Stable lowercase dataset or series name"));
        assert!(output.contains("UTC date of end_time_ns"));
    }

    #[test]
    fn table_listing_includes_quote_bars_and_partition_spec() {
        let output = render_tables();
        assert!(output.contains(
            "market_quote_bars\tidentity(security_type), identity(market), identity(resolution), month(day)"
        ));
    }
}
