use std::collections::HashMap;

use anyhow::{bail, Context, Result};
use arrow::datatypes::DataType;
use clap::{Args, Subcommand};
use futures::StreamExt;
use rlean_core::{Market, SecurityType, Symbol};
use rlean_data_sidecar::{
    CustomQuery, DataSidecarClient, DataSidecarConfig, DeliveryMode, SubscriptionSpec, WireDataType,
};
use rlean_data_tables::{table_definitions, PartitionTransform, TableDefinition};

use crate::config::GlobalConfig;

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

    /// Print the configured sidecar's data manifest
    Manifest {
        /// Emit the sidecar's JSON document without pretty-printing it
        #[arg(long)]
        json: bool,
    },

    /// Query canonical data through a temporary sidecar subscription
    Query(QueryArgs),
}

#[derive(Args)]
pub(crate) struct QueryArgs {
    /// Canonical table name, such as market_trade_bars or custom_points
    table: String,

    /// Symbol to query; optional for provider-wide custom feeds
    symbol: Option<String>,

    /// Provider selection, such as massive, thetadata, or unusual_whales
    #[arg(long)]
    provider: Option<String>,

    /// Provider feed or custom-data series
    #[arg(long)]
    feed: Option<String>,

    /// Physical venue
    #[arg(long, default_value = "")]
    venue: String,

    /// LEAN market
    #[arg(long, default_value = "usa")]
    market: String,

    /// Security type
    #[arg(long, default_value = "equity")]
    security_type: String,

    /// Data resolution
    #[arg(long, default_value = "daily")]
    resolution: String,

    /// Tick type; defaults from the selected table
    #[arg(long)]
    tick_type: Option<String>,

    /// Explicit LEAN SID when it cannot be derived from the symbol
    #[arg(long)]
    sid: Option<u64>,

    /// Inclusive query start, YYYY-MM-DD or RFC 3339
    #[arg(long)]
    start: String,

    /// Inclusive query end, YYYY-MM-DD or RFC 3339
    #[arg(long)]
    end: String,

    /// Include extended-market-hours data
    #[arg(long)]
    extended_market_hours: bool,

    /// Emit newline-delimited JSON rows
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run_data(args: DataArgs) -> Result<()> {
    match args.command {
        DataCommand::Tables => {
            print!("{}", render_tables());
            Ok(())
        }
        DataCommand::Schema { table } => show_schema(&table),
        DataCommand::Manifest { json } => show_manifest(json).await,
        DataCommand::Query(query) => run_query(query).await,
    }
}

async fn sidecar_client() -> Result<DataSidecarClient> {
    let config = GlobalConfig::load()?;
    let endpoint = config.data_sidecar.context(
        "data_sidecar is not configured; run `rlean config set data_sidecar grpc://127.0.0.1:7410`",
    )?;
    DataSidecarClient::connect(DataSidecarConfig {
        endpoint,
        token: config.data_sidecar_token,
        connect_timeout_ms: 10_000,
    })
    .await
}

async fn show_manifest(raw_json: bool) -> Result<()> {
    let manifest = sidecar_client().await?.manifest().await?;
    if manifest.content_type != "application/json" {
        bail!(
            "sidecar returned unsupported manifest content type '{}'",
            manifest.content_type
        );
    }
    let value: serde_json::Value =
        serde_json::from_slice(&manifest.body).context("sidecar manifest is not valid JSON")?;
    if raw_json {
        println!("{}", serde_json::to_string(&value)?);
    } else {
        println!("{}", serde_json::to_string_pretty(&value)?);
    }
    Ok(())
}

async fn run_query(args: QueryArgs) -> Result<()> {
    let spec = query_subscription(&args)?;
    let start_time_ns = parse_query_time(&args.start, false)?;
    let end_time_ns = parse_query_time(&args.end, true)?;
    if start_time_ns > end_time_ns {
        bail!("query start must not be after query end");
    }

    let client = sidecar_client().await?;
    let subscription_id = client
        .add_subscription_spec(spec, DeliveryMode::Backtest)
        .await?;
    let result = async {
        let mut stream = client
            .query(subscription_id, start_time_ns, end_time_ns)
            .await?;
        let mut batches = Vec::new();
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            if args.json {
                let mut writer = arrow::json::LineDelimitedWriter::new(std::io::stdout().lock());
                writer.write(&batch)?;
                writer.finish()?;
            } else {
                batches.push(batch);
            }
        }
        if !args.json && !batches.is_empty() {
            println!("{}", arrow::util::pretty::pretty_format_batches(&batches)?);
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let remove = client.remove_subscription(subscription_id).await;
    result?;
    remove
}

fn query_subscription(args: &QueryArgs) -> Result<SubscriptionSpec> {
    let table = args.table.rsplit('.').next().unwrap_or(&args.table);
    let (data_type, default_tick_type) = table_data_type(table)?;
    let resolution = parse_resolution(&args.resolution)?;
    let tick_type = parse_tick_type(args.tick_type.as_deref().unwrap_or(default_tick_type))?;
    let security_type = parse_security_type(&args.security_type)?;
    let market = Market::new(&args.market);
    let symbol_value = args
        .symbol
        .as_deref()
        .unwrap_or("_ALL")
        .to_ascii_uppercase();
    let symbol_sid = match args.sid {
        Some(sid) => sid,
        None => derived_sid(&symbol_value, security_type, &market, data_type)?,
    };
    let provider = args.provider.clone().unwrap_or_default();
    let feed = args.feed.clone().unwrap_or_default();
    let mut properties = HashMap::new();
    if !provider.is_empty() {
        properties.insert("provider".to_string(), provider.clone());
    }
    if !feed.is_empty() {
        properties.insert("feed".to_string(), feed.clone());
    }
    let custom = matches!(data_type, WireDataType::Custom | WireDataType::Universe);
    let ticker = if custom && args.symbol.is_none() {
        feed.clone()
    } else {
        symbol_value.clone()
    };
    let custom_query = custom.then(|| CustomQuery {
        symbols: args.symbol.clone().into_iter().collect(),
        columns: Vec::new(),
        start_day: None,
        end_day: None,
        start_time_ns: None,
        end_time_ns: None,
        string_equals: HashMap::new(),
        string_in: Vec::new(),
        numeric_min: HashMap::new(),
        numeric_max: HashMap::new(),
        properties: HashMap::new(),
    });

    Ok(SubscriptionSpec {
        config_id: 1,
        symbol_sid,
        symbol_value: symbol_value.clone(),
        permanent_ticker: symbol_value,
        security_type: security_type as i32,
        market: market.as_str().to_string(),
        resolution,
        tick_type,
        data_type: data_type as i32,
        extended_market_hours: args.extended_market_hours,
        source_type: provider,
        ticker,
        custom_query,
        properties,
        venue: args.venue.clone(),
    })
}

fn table_data_type(table: &str) -> Result<(WireDataType, &'static str)> {
    let value = match table {
        "market_trade_bars" => (WireDataType::TradeBar, "trade"),
        "market_quote_bars" => (WireDataType::QuoteBar, "quote"),
        "market_ticks" => (WireDataType::Tick, "trade"),
        "margin_interest" => (WireDataType::MarginInterestRate, "trade"),
        "custom_points" => (WireDataType::Custom, "trade"),
        "option_universe" => (WireDataType::OptionUniverse, "quote"),
        "future_universe" => (WireDataType::FutureUniverse, "quote"),
        "fundamental_universe" => (WireDataType::FundamentalUniverse, "trade"),
        "etf_constituents" => (WireDataType::EtfConstituent, "trade"),
        "factor_files" => (WireDataType::FactorFile, "trade"),
        "map_files" => (WireDataType::MapFile, "trade"),
        _ => bail!("unknown or non-queryable rlean data table '{table}'"),
    };
    Ok(value)
}

fn parse_resolution(value: &str) -> Result<i32> {
    match value.to_ascii_lowercase().as_str() {
        "tick" => Ok(0),
        "second" => Ok(1),
        "minute" => Ok(2),
        "hour" => Ok(3),
        "daily" | "day" => Ok(4),
        _ => bail!("invalid resolution '{value}'"),
    }
}

fn parse_tick_type(value: &str) -> Result<i32> {
    match value.to_ascii_lowercase().as_str() {
        "trade" => Ok(0),
        "quote" => Ok(1),
        "open_interest" | "open-interest" => Ok(2),
        _ => bail!("invalid tick type '{value}'"),
    }
}

fn parse_security_type(value: &str) -> Result<SecurityType> {
    match value.to_ascii_lowercase().as_str() {
        "base" => Ok(SecurityType::Base),
        "equity" => Ok(SecurityType::Equity),
        "option" => Ok(SecurityType::Option),
        "forex" => Ok(SecurityType::Forex),
        "future" => Ok(SecurityType::Future),
        "crypto" => Ok(SecurityType::Crypto),
        "index" => Ok(SecurityType::Index),
        "crypto_future" | "crypto-future" => Ok(SecurityType::CryptoFuture),
        _ => bail!("invalid security type '{value}'"),
    }
}

fn derived_sid(
    ticker: &str,
    security_type: SecurityType,
    market: &Market,
    data_type: WireDataType,
) -> Result<u64> {
    let symbol = if matches!(data_type, WireDataType::Custom | WireDataType::Universe) {
        Symbol::create_base("custom", ticker, market)
    } else {
        match security_type {
            SecurityType::Equity => Symbol::create_equity(ticker, market),
            SecurityType::Forex => Symbol::create_forex(ticker),
            SecurityType::Crypto => Symbol::create_crypto(ticker, market),
            SecurityType::CryptoFuture => Symbol::create_crypto_future(ticker, market),
            SecurityType::Index => Symbol::create_index(ticker, market),
            SecurityType::Base => Symbol::create_base("custom", ticker, market),
            other => bail!("cannot derive a SID for {other}; pass --sid"),
        }
    };
    Ok(symbol.id.sid)
}

fn parse_query_time(value: &str, end_of_day: bool) -> Result<i64> {
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        let time = if end_of_day {
            date.and_hms_nano_opt(23, 59, 59, 999_999_999)
        } else {
            date.and_hms_opt(0, 0, 0)
        }
        .context("invalid query date")?;
        return Ok(time
            .and_utc()
            .timestamp_nanos_opt()
            .context("query date is out of range")?);
    }
    let time = chrono::DateTime::parse_from_rfc3339(value)
        .with_context(|| format!("invalid time '{value}', expected YYYY-MM-DD or RFC 3339"))?;
    time.timestamp_nanos_opt()
        .context("query time is out of range")
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

    #[test]
    fn custom_query_maps_direct_cli_arguments_to_subscription_contract() {
        let args = QueryArgs {
            table: "custom_points".into(),
            symbol: Some("SPY".into()),
            provider: Some("unusual_whales".into()),
            feed: Some("flow_alerts".into()),
            venue: "unusual_whales".into(),
            market: "usa".into(),
            security_type: "equity".into(),
            resolution: "tick".into(),
            tick_type: None,
            sid: None,
            start: "2026-07-01".into(),
            end: "2026-07-15".into(),
            extended_market_hours: false,
            json: true,
        };
        let spec = query_subscription(&args).unwrap();
        assert_eq!(spec.data_type, WireDataType::Custom as i32);
        assert_eq!(spec.source_type, "unusual_whales");
        assert_eq!(spec.ticker, "SPY");
        assert_eq!(spec.properties.get("feed").unwrap(), "flow_alerts");
        assert_eq!(spec.venue, "unusual_whales");
    }

    #[test]
    fn date_query_end_includes_the_entire_day() {
        let start = parse_query_time("2026-07-15", false).unwrap();
        let end = parse_query_time("2026-07-15", true).unwrap();
        assert_eq!(end - start, 86_400_000_000_000 - 1);
    }
}
