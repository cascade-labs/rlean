use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use arrow_array::{
    Array, BooleanArray, Decimal128Array, Int32Array, Int64Array, LargeStringArray, RecordBatch,
    StringArray, StringViewArray, UInt8Array,
};
use rlean_core::{Market, NanosecondTimestamp, Symbol, TickType, TimeSpan};
use rlean_data::FundamentalData;
use rlean_data_tables::{
    Bar, CustomDataPoint, DataMappingMode, FactorFileEntry, MapFileEntry, QuoteBar,
    RiskFreeInterestRate, Tick, TradeBar, DECIMAL_SCALE,
};
use rust_decimal::Decimal;

use crate::WireDataType;

#[derive(Debug, Clone)]
pub enum CanonicalDataBatch {
    TradeBars(Vec<TradeBar>),
    QuoteBars(Vec<QuoteBar>),
    Ticks(Vec<Tick>),
    Custom(Vec<CustomDataPoint>),
    Universe(Vec<CustomDataPoint>),
    Fundamentals(Vec<FundamentalData>),
    RiskFreeInterestRates(Vec<RiskFreeInterestRate>),
    /// The contract remains available to callers that consume a canonical
    /// universe or auxiliary table directly.
    RecordBatch(RecordBatch),
}

pub fn decode_batch(
    data_type: WireDataType,
    batch: RecordBatch,
    symbol: &Symbol,
) -> anyhow::Result<CanonicalDataBatch> {
    match data_type {
        WireDataType::TradeBar => Ok(CanonicalDataBatch::TradeBars(decode_trade_bars(
            &batch, symbol,
        )?)),
        WireDataType::QuoteBar => Ok(CanonicalDataBatch::QuoteBars(decode_quote_bars(
            &batch, symbol,
        )?)),
        WireDataType::Tick | WireDataType::OpenInterest => {
            Ok(CanonicalDataBatch::Ticks(decode_ticks(&batch, symbol)?))
        }
        WireDataType::Custom => Ok(CanonicalDataBatch::Custom(decode_custom(&batch)?)),
        WireDataType::Universe => Ok(CanonicalDataBatch::Universe(decode_custom(&batch)?)),
        WireDataType::FundamentalUniverse => Ok(CanonicalDataBatch::Fundamentals(
            decode_fundamentals(&batch)?,
        )),
        WireDataType::RiskFreeInterestRate => Ok(CanonicalDataBatch::RiskFreeInterestRates(
            decode_risk_free_interest_rates(&batch)?,
        )),
        _ => Ok(CanonicalDataBatch::RecordBatch(batch)),
    }
}

fn decode_risk_free_interest_rates(
    batch: &RecordBatch,
) -> anyhow::Result<Vec<RiskFreeInterestRate>> {
    let time = int64(batch, "time_ns")?;
    let annual_rate = decimal(batch, "annual_rate")?;
    Ok((0..batch.num_rows())
        .map(|row| RiskFreeInterestRate {
            time: NanosecondTimestamp(time.value(row)),
            annual_rate: decimal_value(annual_rate.value(row)),
            venue: string_at(batch, "venue", row),
        })
        .collect())
}

pub fn decode_factor_file_batch(batch: &RecordBatch) -> anyhow::Result<Vec<FactorFileEntry>> {
    let dates = int64(batch, "date_ns")?;
    let price_factor = decimal(batch, "price_factor")?;
    let split_factor = decimal(batch, "split_factor")?;
    let reference_price = decimal(batch, "reference_price")?;
    Ok((0..batch.num_rows())
        .map(|row| FactorFileEntry {
            date: NanosecondTimestamp(dates.value(row)).date_utc(),
            price_factor: decimal_value(price_factor.value(row)),
            split_factor: decimal_value(split_factor.value(row)),
            reference_price: decimal_value(reference_price.value(row)),
        })
        .collect())
}

pub fn decode_map_file_batch(batch: &RecordBatch) -> anyhow::Result<Vec<MapFileEntry>> {
    let dates = int64(batch, "date_ns")?;
    let mapped_symbol = required_strings(batch, "mapped_symbol")?;
    let primary_exchange = required_strings(batch, "primary_exchange_code")?;
    let mapping_mode = batch
        .column_by_name("data_mapping_mode")
        .and_then(|column| column.as_any().downcast_ref::<Int32Array>());
    Ok((0..batch.num_rows())
        .map(|row| MapFileEntry {
            date: NanosecondTimestamp(dates.value(row)).date_utc(),
            mapped_symbol: mapped_symbol.value(row).to_string(),
            primary_exchange_code: primary_exchange.value(row).to_string(),
            data_mapping_mode: mapping_mode
                .filter(|values| !values.is_null(row))
                .and_then(|values| DataMappingMode::try_from(values.value(row)).ok()),
        })
        .collect())
}

fn decode_trade_bars(batch: &RecordBatch, symbol: &Symbol) -> anyhow::Result<Vec<TradeBar>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let open = decimal(batch, "open")?;
    let high = decimal(batch, "high")?;
    let low = decimal(batch, "low")?;
    let close = decimal(batch, "close")?;
    let volume = decimal(batch, "volume")?;
    let period = int64(batch, "period_ns")?;
    Ok((0..batch.num_rows())
        .map(|row| TradeBar {
            symbol: symbol.clone(),
            venue: string_at(batch, "venue", row),
            time: NanosecondTimestamp(time.value(row)),
            end_time: NanosecondTimestamp(end_time.value(row)),
            open: decimal_value(open.value(row)),
            high: decimal_value(high.value(row)),
            low: decimal_value(low.value(row)),
            close: decimal_value(close.value(row)),
            volume: decimal_value(volume.value(row)),
            period: TimeSpan::from_nanos(period.value(row)),
        })
        .collect())
}

fn decode_quote_bars(batch: &RecordBatch, symbol: &Symbol) -> anyhow::Result<Vec<QuoteBar>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let bid_open = decimal(batch, "bid_open")?;
    let bid_high = decimal(batch, "bid_high")?;
    let bid_low = decimal(batch, "bid_low")?;
    let bid_close = decimal(batch, "bid_close")?;
    let ask_open = decimal(batch, "ask_open")?;
    let ask_high = decimal(batch, "ask_high")?;
    let ask_low = decimal(batch, "ask_low")?;
    let ask_close = decimal(batch, "ask_close")?;
    let bid_size = decimal(batch, "last_bid_size")?;
    let ask_size = decimal(batch, "last_ask_size")?;
    let period = int64(batch, "period_ns")?;
    Ok((0..batch.num_rows())
        .map(|row| QuoteBar {
            symbol: symbol.clone(),
            venue: string_at(batch, "venue", row),
            time: NanosecondTimestamp(time.value(row)),
            end_time: NanosecondTimestamp(end_time.value(row)),
            bid: bar_at(bid_open, bid_high, bid_low, bid_close, row),
            ask: bar_at(ask_open, ask_high, ask_low, ask_close, row),
            last_bid_size: decimal_value(bid_size.value(row)),
            last_ask_size: decimal_value(ask_size.value(row)),
            period: TimeSpan::from_nanos(period.value(row)),
        })
        .collect())
}

fn bar_at(
    open: &Decimal128Array,
    high: &Decimal128Array,
    low: &Decimal128Array,
    close: &Decimal128Array,
    row: usize,
) -> Option<Bar> {
    if open.is_null(row) || high.is_null(row) || low.is_null(row) || close.is_null(row) {
        return None;
    }
    Some(Bar::new(
        decimal_value(open.value(row)),
        decimal_value(high.value(row)),
        decimal_value(low.value(row)),
        decimal_value(close.value(row)),
    ))
}

fn decode_ticks(batch: &RecordBatch, symbol: &Symbol) -> anyhow::Result<Vec<Tick>> {
    let time = int64(batch, "time_ns")?;
    let value = decimal(batch, "value")?;
    let quantity = decimal(batch, "quantity")?;
    let bid_price = decimal(batch, "bid_price")?;
    let ask_price = decimal(batch, "ask_price")?;
    let bid_size = decimal(batch, "bid_size")?;
    let ask_size = decimal(batch, "ask_size")?;
    let suspicious = batch
        .column_by_name("suspicious")
        .and_then(|column| column.as_any().downcast_ref::<BooleanArray>());
    Ok((0..batch.num_rows())
        .map(|row| Tick {
            symbol: symbol.clone(),
            venue: string_at(batch, "venue", row),
            time: NanosecondTimestamp(time.value(row)),
            tick_type: tick_type_at(batch, row),
            value: decimal_value(value.value(row)),
            quantity: decimal_value(quantity.value(row)),
            bid_price: decimal_value(bid_price.value(row)),
            ask_price: decimal_value(ask_price.value(row)),
            bid_size: decimal_value(bid_size.value(row)),
            ask_size: decimal_value(ask_size.value(row)),
            exchange: string_at(batch, "exchange", row),
            sale_condition: string_at(batch, "sale_condition", row),
            suspicious: suspicious.map(|array| array.value(row)).unwrap_or(false),
        })
        .collect())
}

fn decode_custom(batch: &RecordBatch) -> anyhow::Result<Vec<CustomDataPoint>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let value = decimal(batch, "value")?;
    Ok((0..batch.num_rows())
        .map(|row| {
            let fields = string_at(batch, "fields_json", row)
                .and_then(|json| {
                    serde_json::from_str::<HashMap<String, serde_json::Value>>(&json).ok()
                })
                .unwrap_or_default();
            CustomDataPoint {
                time: NanosecondTimestamp(time.value(row)),
                end_time: NanosecondTimestamp(end_time.value(row)),
                value: decimal_value(value.value(row)),
                venue: string_at(batch, "venue", row),
                symbol: string_at(batch, "symbol_value", row),
                fields: Arc::new(fields),
            }
        })
        .collect())
}

/// Decode the canonical point-in-time fundamental snapshot table.  The
/// sidecar only emits rows whose `end_time_ns` has been reached, so this
/// conversion intentionally carries that availability timestamp through to
/// the engine rather than deriving it from a period end date.
fn decode_fundamentals(batch: &RecordBatch) -> anyhow::Result<Vec<FundamentalData>> {
    let time = int64(batch, "time_ns")?;
    let end_time = int64(batch, "end_time_ns")?;
    let markets = required_strings(batch, "market")?;
    let symbols = required_strings(batch, "symbol_value")?;
    let volumes = decimal(batch, "volume")?;
    let dollar_volumes = decimal(batch, "dollar_volume")?;
    let market_caps = decimal(batch, "market_cap")?;

    Ok((0..batch.num_rows())
        .map(|row| {
            let symbol =
                Symbol::create_equity(symbols.value(row), &Market::new(markets.value(row)));
            let mut data = FundamentalData::new(symbol, NanosecondTimestamp(time.value(row)));
            data.end_time = NanosecondTimestamp(end_time.value(row));
            data.volume = Some(decimal_value(volumes.value(row)));
            data.dollar_volume = Some(decimal_value(dollar_volumes.value(row)));
            data.market_cap = Some(decimal_value(market_caps.value(row)));
            data
        })
        .collect())
}

fn int64<'a>(batch: &'a RecordBatch, name: &str) -> anyhow::Result<&'a Int64Array> {
    batch
        .column_by_name(name)
        .with_context(|| format!("Flight batch is missing {name}"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("Flight column {name} must be int64"))
}

fn decimal<'a>(batch: &'a RecordBatch, name: &str) -> anyhow::Result<&'a Decimal128Array> {
    batch
        .column_by_name(name)
        .with_context(|| format!("Flight batch is missing {name}"))?
        .as_any()
        .downcast_ref::<Decimal128Array>()
        .ok_or_else(|| anyhow!("Flight column {name} must be decimal128"))
}

fn required_strings<'a>(batch: &'a RecordBatch, name: &str) -> anyhow::Result<&'a StringArray> {
    batch
        .column_by_name(name)
        .with_context(|| format!("Flight batch is missing {name}"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("Flight column {name} must be utf8"))
}

fn decimal_value(value: i128) -> Decimal {
    Decimal::from_i128_with_scale(value, DECIMAL_SCALE as u32)
}

fn tick_type_at(batch: &RecordBatch, row: usize) -> TickType {
    let column = batch.column_by_name("tick_type");
    let value = column
        .and_then(|column| column.as_any().downcast_ref::<UInt8Array>())
        .map(|array| array.value(row) as i32)
        .or_else(|| {
            column
                .and_then(|column| column.as_any().downcast_ref::<Int32Array>())
                .map(|array| array.value(row))
        })
        .unwrap_or_default();
    match value {
        1 => TickType::Quote,
        2 => TickType::OpenInterest,
        _ => TickType::Trade,
    }
}

fn string_at(batch: &RecordBatch, name: &str, row: usize) -> Option<String> {
    let array = batch.column_by_name(name)?;
    if array.is_null(row) {
        return None;
    }
    array
        .as_any()
        .downcast_ref::<StringArray>()
        .map(|array| array.value(row).to_string())
        .or_else(|| {
            array
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .map(|array| array.value(row).to_string())
        })
        .or_else(|| {
            array
                .as_any()
                .downcast_ref::<StringViewArray>()
                .map(|array| array.value(row).to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, Date32Array};
    use rlean_core::Market;
    use rlean_data_tables::{FundamentalUniverseRow, TableContract};

    fn decimal(values: Vec<Option<i128>>) -> Decimal128Array {
        Decimal128Array::from(values)
            .with_precision_and_scale(
                rlean_data_tables::DECIMAL_PRECISION,
                rlean_data_tables::DECIMAL_SCALE,
            )
            .unwrap()
    }

    #[test]
    fn decodes_point_in_time_fundamental_snapshot() {
        let scale = 10_i128.pow(rlean_data_tables::DECIMAL_SCALE as u32);
        let batch = RecordBatch::try_new(
            FundamentalUniverseRow::schema(),
            vec![
                Arc::new(Date32Array::from(vec![19_758])) as ArrayRef,
                Arc::new(Int64Array::from(vec![1_706_897_600_000_000_000])),
                Arc::new(Int64Array::from(vec![1_707_130_600_000_000_000])),
                Arc::new(StringArray::from(vec!["usa"])),
                Arc::new(Int64Array::from(vec![42_i64])),
                Arc::new(StringArray::from(vec!["ABC"])),
                Arc::new(decimal(vec![Some(1_000_000 * scale)])),
                Arc::new(decimal(vec![Some(20_000_000 * scale)])),
                Arc::new(decimal(vec![Some(200_000_000 * scale)])),
            ],
        )
        .unwrap();
        let base = Symbol::create_base("fundamental_universe", "massive", &Market::usa());
        let CanonicalDataBatch::Fundamentals(rows) =
            decode_batch(WireDataType::FundamentalUniverse, batch, &base).unwrap()
        else {
            panic!("fundamental wire type must decode to typed rows")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].symbol.value.as_ref(), "ABC");
        assert_eq!(rows[0].market_cap, Some(Decimal::from(200_000_000)));
        assert_eq!(rows[0].volume, Some(Decimal::from(1_000_000)));
        assert_eq!(rows[0].dollar_volume, Some(Decimal::from(20_000_000)));
        assert_eq!(rows[0].end_time.0, 1_707_130_600_000_000_000);
    }
}
