use arrow_schema::{DataType, Field, Schema};
use chrono::NaiveDate;
use rust_decimal::Decimal;
use std::sync::Arc;

/// Arrow schema for TradeBar parquet files.
/// Uses INT64 nanosecond timestamps and INT64 scaled prices (×10^8 for precision).
pub fn trade_bar_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("time_ns", DataType::Int64, false),
        Field::new("end_time_ns", DataType::Int64, false),
        Field::new("symbol_sid", DataType::UInt64, false),
        Field::new("symbol_value", DataType::Utf8, false),
        // Prices stored as i64, scaled by PRICE_SCALE (1e8) to preserve 8 decimal places
        Field::new("open", DataType::Int64, false),
        Field::new("high", DataType::Int64, false),
        Field::new("low", DataType::Int64, false),
        Field::new("close", DataType::Int64, false),
        Field::new("volume", DataType::Int64, false),
        Field::new("period_ns", DataType::Int64, false),
    ]))
}

/// Arrow schema for QuoteBar parquet files.
pub fn quote_bar_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("time_ns", DataType::Int64, false),
        Field::new("end_time_ns", DataType::Int64, false),
        Field::new("symbol_sid", DataType::UInt64, false),
        Field::new("symbol_value", DataType::Utf8, false),
        Field::new("bid_open", DataType::Int64, true),
        Field::new("bid_high", DataType::Int64, true),
        Field::new("bid_low", DataType::Int64, true),
        Field::new("bid_close", DataType::Int64, true),
        Field::new("ask_open", DataType::Int64, true),
        Field::new("ask_high", DataType::Int64, true),
        Field::new("ask_low", DataType::Int64, true),
        Field::new("ask_close", DataType::Int64, true),
        Field::new("last_bid_size", DataType::Int64, false),
        Field::new("last_ask_size", DataType::Int64, false),
        Field::new("period_ns", DataType::Int64, false),
    ]))
}

/// Arrow schema for Tick parquet files.
pub fn tick_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("time_ns", DataType::Int64, false),
        Field::new("symbol_sid", DataType::UInt64, false),
        Field::new("symbol_value", DataType::Utf8, false),
        Field::new("tick_type", DataType::UInt8, false),
        Field::new("value", DataType::Int64, false),
        Field::new("quantity", DataType::Int64, false),
        Field::new("bid_price", DataType::Int64, false),
        Field::new("ask_price", DataType::Int64, false),
        Field::new("bid_size", DataType::Int64, false),
        Field::new("ask_size", DataType::Int64, false),
        Field::new("exchange", DataType::Utf8, true),
        Field::new("sale_condition", DataType::Utf8, true),
        Field::new("suspicious", DataType::Boolean, false),
    ]))
}

/// Arrow schema for OpenInterest parquet files.
pub fn open_interest_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("time_ns", DataType::Int64, false),
        Field::new("symbol_sid", DataType::UInt64, false),
        Field::new("symbol_value", DataType::Utf8, false),
        Field::new("value", DataType::Int64, false),
    ]))
}

/// Arrow schema for OptionEodBar parquet files.
///
/// All contracts for one underlying live in a single file, keyed by underlying
/// ticker (e.g. `spy`).  Rows are identified by `date_ns` (nanoseconds since
/// Unix epoch at midnight UTC) so DataFusion predicate pushdown can skip
/// row-groups for other dates.
///
/// Prices are scaled by PRICE_SCALE (×1e8).  Volume and size columns are
/// raw i64 (no scaling).
pub fn option_eod_bar_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("symbol_value", DataType::Utf8, false),   // full OSI ticker
        Field::new("underlying", DataType::Utf8, false),      // underlying ticker
        Field::new("expiration_ns", DataType::Int64, false),
        Field::new("strike", DataType::Int64, false),         // ×1e8
        Field::new("right", DataType::Utf8, false),           // "C" or "P"
        Field::new("open", DataType::Int64, false),           // ×1e8
        Field::new("high", DataType::Int64, false),           // ×1e8
        Field::new("low", DataType::Int64, false),            // ×1e8
        Field::new("close", DataType::Int64, false),          // ×1e8
        Field::new("volume", DataType::Int64, false),         // raw shares
        Field::new("bid", DataType::Int64, false),            // ×1e8
        Field::new("ask", DataType::Int64, false),            // ×1e8
        Field::new("bid_size", DataType::Int64, false),       // raw contracts
        Field::new("ask_size", DataType::Int64, false),       // raw contracts
    ]))
}

/// Arrow schema for option universe parquet files.
///
/// One row per contract in the universe for a given underlying + date.
/// Used to enumerate which contracts were listed on a given day.
pub fn option_universe_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("symbol_value", DataType::Utf8, false),   // full OSI ticker
        Field::new("underlying", DataType::Utf8, false),
        Field::new("expiration_ns", DataType::Int64, false),
        Field::new("strike", DataType::Int64, false),         // ×1e8
        Field::new("right", DataType::Utf8, false),           // "C" or "P"
    ]))
}

/// Price scale: multiply Decimal by this before converting to i64.
/// Gives 8 decimal places of precision.
pub const PRICE_SCALE: i64 = 100_000_000; // 1e8

/// Convert a rust_decimal::Decimal to a scaled i64.
pub fn price_to_i64(price: &rust_decimal::Decimal) -> i64 {
    use rust_decimal::prelude::ToPrimitive;
    let scaled = price * rust_decimal::Decimal::from(PRICE_SCALE);
    scaled.round().to_i64().unwrap_or(0)
}

/// Convert a scaled i64 back to rust_decimal::Decimal.
pub fn i64_to_price(raw: i64) -> rust_decimal::Decimal {
    rust_decimal::Decimal::from(raw) / rust_decimal::Decimal::from(PRICE_SCALE)
}

/// Convert a `NaiveDate` to nanoseconds since Unix epoch (midnight UTC).
pub fn date_to_ns(date: chrono::NaiveDate) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp_nanos_opt().unwrap_or(0))
        .unwrap_or(0)
}

/// Reconstruct a `NaiveDate` from nanoseconds since Unix epoch.
pub fn ns_to_date(ns: i64) -> chrono::NaiveDate {
    use chrono::TimeZone;
    let secs = ns / 1_000_000_000;
    chrono::Utc
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.date_naive())
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
}

// ─── Option data types ─────────────────────────────────────────────────────

/// A single option contract's end-of-day bar.
///
/// One Parquet row in an option EOD bar file.  All files for the same
/// underlying are grouped together; the specific contract is identified
/// by `symbol_value` (the full OSI ticker string).
#[derive(Debug, Clone, PartialEq)]
pub struct OptionEodBar {
    /// Date of the bar.
    pub date: NaiveDate,
    /// Full OSI option ticker, e.g. `SPY210430P00480000`.
    pub symbol_value: String,
    /// Underlying ticker, e.g. `SPY`.
    pub underlying: String,
    /// Option expiration date.
    pub expiration: NaiveDate,
    /// Strike price.
    pub strike: Decimal,
    /// Option right: `"C"` (call) or `"P"` (put).
    pub right: String,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    /// Trade volume in raw contracts (not scaled).
    pub volume: i64,
    /// Best bid price.
    pub bid: Decimal,
    /// Best ask price.
    pub ask: Decimal,
    /// Bid size in raw contracts.
    pub bid_size: i64,
    /// Ask size in raw contracts.
    pub ask_size: i64,
}

/// A single row in an option universe file.
///
/// Lists which contracts were in the tradeable universe for a given
/// underlying on a given date.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionUniverseRow {
    /// Date of the universe snapshot.
    pub date: NaiveDate,
    /// Full OSI option ticker.
    pub symbol_value: String,
    /// Underlying ticker.
    pub underlying: String,
    /// Expiration date.
    pub expiration: NaiveDate,
    /// Strike price.
    pub strike: Decimal,
    /// Option right: `"C"` or `"P"`.
    pub right: String,
}

// ─── Factor file / Map file ───────────────────────────────────────────────────

/// Arrow schema for factor file parquet files.
///
/// One row per date entry, mirroring LEAN's CSV format:
///   `{yyyyMMdd},{price_factor},{split_factor},{reference_price}`
///
/// Dates are stored as nanoseconds since Unix epoch (midnight UTC).
/// Factors are `Float64` to preserve the 7–8 decimal places that LEAN uses.
pub fn factor_file_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("date_ns",         DataType::Int64,   false),
        Field::new("price_factor",    DataType::Float64, false),
        Field::new("split_factor",    DataType::Float64, false),
        Field::new("reference_price", DataType::Float64, false),
    ]))
}

/// Arrow schema for map file parquet files.
///
/// One row per date/ticker pair, mirroring LEAN's CSV format:
///   `{yyyyMMdd},{ticker}`
///
/// Dates are stored as nanoseconds since Unix epoch (midnight UTC).
pub fn map_file_schema() -> Arc<Schema> {
    Arc::new(Schema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("ticker",  DataType::Utf8,  false),
    ]))
}

/// One row in a LEAN factor file (equity price-adjustment factors).
///
/// LEAN CSV format per row:
///   `{yyyyMMdd},{price_factor},{split_factor},{reference_price}`
///
/// - `price_factor`    – cumulative dividend price-adjustment factor
/// - `split_factor`    – cumulative split factor
/// - `reference_price` – raw closing price the day before the event (0 when unknown)
#[derive(Debug, Clone, PartialEq)]
pub struct FactorFileEntry {
    /// The date these factors apply (factors applied backward from this date).
    pub date: NaiveDate,
    /// Cumulative dividend price-adjustment factor.
    pub price_factor: f64,
    /// Cumulative split factor.
    pub split_factor: f64,
    /// Raw reference price (closing price before the event; 0 if not recorded).
    pub reference_price: f64,
}

impl FactorFileEntry {
    /// Convert `date` to nanoseconds since Unix epoch (midnight UTC).
    pub fn date_ns(&self) -> i64 {
        date_to_ns(self.date)
    }
}

/// One row in a LEAN map file (ticker rename / mapping history).
///
/// LEAN CSV format per row:
///   `{yyyyMMdd},{ticker}` (ticker is lowercase in the file)
///
/// Semantics: this ticker was valid FROM this date forward (until the next row's date).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapFileEntry {
    /// The date from which this ticker mapping becomes valid.
    pub date: NaiveDate,
    /// The ticker symbol valid from this date (stored uppercase by convention).
    pub ticker: String,
}

impl MapFileEntry {
    /// Convert `date` to nanoseconds since Unix epoch (midnight UTC).
    pub fn date_ns(&self) -> i64 {
        date_to_ns(self.date)
    }
}
