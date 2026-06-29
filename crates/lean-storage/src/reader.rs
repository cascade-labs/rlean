use crate::predicate::QueryParams;
use crate::schema::{
    i64_to_price, ns_to_date, FactorFileEntry, MapFileEntry, OptionEodBar, OptionUniverseRow,
};
use arrow_array::{
    Array, BooleanArray, Int32Array, Int64Array, RecordBatch, StringArray, UInt64Array, UInt8Array,
};
use chrono::NaiveDate;
use lean_core::{
    DateTime, LeanError, NanosecondTimestamp, Result as LeanResult, SecurityType, Symbol, TickType,
    TimeSpan,
};
use lean_data::{Bar, QuoteBar, Tick, TradeBar};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Compatibility Parquet reader retained for provider plugins that still write
/// legacy date-partitioned cache files.
pub struct ParquetReader;

impl ParquetReader {
    pub fn new() -> Self {
        Self
    }

    pub fn read_trade_bar_partition(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<TradeBar>> {
        let mut out = Vec::new();
        for batch in record_batches(path)? {
            append_trade_bars(&batch, template, params, &mut out)?;
        }
        out.sort_by_key(|bar| (bar.time.0, bar.symbol.id.sid));
        Ok(limit(out, params))
    }

    pub fn read_quote_bar_partition(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<QuoteBar>> {
        let mut out = Vec::new();
        for batch in record_batches(path)? {
            append_quote_bars(&batch, template, params, &mut out)?;
        }
        out.sort_by_key(|bar| (bar.time.0, bar.symbol.id.sid));
        Ok(limit(out, params))
    }

    pub fn read_tick_partition(
        &self,
        path: &Path,
        template: &Symbol,
        params: &QueryParams,
    ) -> LeanResult<Vec<Tick>> {
        let mut out = Vec::new();
        for batch in record_batches(path)? {
            append_ticks(&batch, template, params, &mut out)?;
        }
        out.sort_by_key(|tick| (tick.time.0, tick.symbol.id.sid));
        Ok(limit(out, params))
    }

    pub fn read_margin_interest_rate_partition(
        &self,
        _path: &Path,
        _template: &Symbol,
        _params: &QueryParams,
    ) -> LeanResult<Vec<lean_data::MarginInterestRate>> {
        Ok(Vec::new())
    }

    pub fn read_perpetual_context_partition(
        &self,
        _path: &Path,
        _template: &Symbol,
        _params: &QueryParams,
    ) -> LeanResult<Vec<lean_data::PerpetualContext>> {
        Ok(Vec::new())
    }

    pub fn read_option_universe(&self, paths: &[PathBuf]) -> LeanResult<Vec<OptionUniverseRow>> {
        let mut out = Vec::new();
        for path in paths {
            for batch in record_batches(path)? {
                out.extend(crate::convert::record_batch_to_option_universe_rows(&batch));
            }
        }
        Ok(out)
    }

    pub async fn read_option_eod_partition_grouped(
        &self,
        path: &Path,
        underlyings: &[String],
    ) -> LeanResult<HashMap<String, Vec<OptionEodBar>>> {
        let wanted = underlyings
            .iter()
            .map(|value| value.to_ascii_uppercase())
            .collect::<HashSet<_>>();
        let mut grouped: HashMap<String, Vec<OptionEodBar>> = HashMap::new();
        for batch in record_batches(path)? {
            append_option_eod(&batch, &wanted, &mut grouped)?;
        }
        for rows in grouped.values_mut() {
            rows.sort_by_key(|row| (row.date, row.expiration, row.strike, row.right.clone()));
        }
        Ok(grouped)
    }

    pub fn read_factor_file(&self, path: &Path) -> LeanResult<Vec<FactorFileEntry>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for batch in record_batches(path)? {
            append_factor_rows(&batch, &mut out)?;
        }
        out.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(out)
    }

    pub fn read_map_file(&self, path: &Path) -> LeanResult<Vec<MapFileEntry>> {
        if !path.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for batch in record_batches(path)? {
            append_map_rows(&batch, &mut out)?;
        }
        out.sort_by_key(|row| row.date);
        Ok(out)
    }
}

impl Default for ParquetReader {
    fn default() -> Self {
        Self::new()
    }
}

fn record_batches(path: &Path) -> LeanResult<Vec<RecordBatch>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = std::fs::File::open(path)
        .map_err(|e| LeanError::DataError(format!("{}: {}", path.display(), e)))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)
        .map_err(|e| LeanError::DataError(e.to_string()))?
        .build()
        .map_err(|e| LeanError::DataError(e.to_string()))?;
    reader
        .map(|batch| batch.map_err(|e| LeanError::DataError(e.to_string())))
        .collect()
}

fn append_trade_bars(
    batch: &RecordBatch,
    template: &Symbol,
    params: &QueryParams,
    out: &mut Vec<TradeBar>,
) -> LeanResult<()> {
    let time = int64_named(batch, "time_ns")?;
    let end_time = int64_named(batch, "end_time_ns")?;
    let sid = sid_named(batch, "symbol_sid")?;
    let value = string_named(batch, "symbol_value")?;
    let open = int64_named(batch, "open")?;
    let high = int64_named(batch, "high")?;
    let low = int64_named(batch, "low")?;
    let close = int64_named(batch, "close")?;
    let volume = int64_named(batch, "volume")?;
    let period = int64_named(batch, "period_ns")?;
    for row in 0..batch.num_rows() {
        let symbol_sid = sid.value(row);
        let bar_time = NanosecondTimestamp(time.value(row));
        let bar_end = NanosecondTimestamp(end_time.value(row));
        if !matches_sid(symbol_sid, params)
            || !matches_time(bar_time, params)
            || !matches_bar_time(bar_end, params)
        {
            continue;
        }
        let symbol = symbol_for_row(template, value.value(row), symbol_sid);
        out.push(TradeBar {
            symbol,
            time: bar_time,
            end_time: bar_end,
            open: i64_to_price(open.value(row)),
            high: i64_to_price(high.value(row)),
            low: i64_to_price(low.value(row)),
            close: i64_to_price(close.value(row)),
            volume: i64_to_price(volume.value(row)),
            period: TimeSpan::from_nanos(period.value(row)),
        });
    }
    Ok(())
}

fn append_quote_bars(
    batch: &RecordBatch,
    template: &Symbol,
    params: &QueryParams,
    out: &mut Vec<QuoteBar>,
) -> LeanResult<()> {
    let time = int64_named(batch, "time_ns")?;
    let end_time = int64_named(batch, "end_time_ns")?;
    let sid = sid_named(batch, "symbol_sid")?;
    let value = string_named(batch, "symbol_value")?;
    let bid_open = int64_named(batch, "bid_open")?;
    let bid_high = int64_named(batch, "bid_high")?;
    let bid_low = int64_named(batch, "bid_low")?;
    let bid_close = int64_named(batch, "bid_close")?;
    let ask_open = int64_named(batch, "ask_open")?;
    let ask_high = int64_named(batch, "ask_high")?;
    let ask_low = int64_named(batch, "ask_low")?;
    let ask_close = int64_named(batch, "ask_close")?;
    let bid_size = int64_named(batch, "last_bid_size")?;
    let ask_size = int64_named(batch, "last_ask_size")?;
    let period = int64_named(batch, "period_ns")?;
    for row in 0..batch.num_rows() {
        let symbol_sid = sid.value(row);
        let bar_time = NanosecondTimestamp(time.value(row));
        let bar_end = NanosecondTimestamp(end_time.value(row));
        if !matches_sid(symbol_sid, params)
            || !matches_time(bar_time, params)
            || !matches_bar_time(bar_end, params)
        {
            continue;
        }
        let bid = (!bid_close.is_null(row)).then(|| Bar {
            open: i64_to_price(bid_open.value(row)),
            high: i64_to_price(bid_high.value(row)),
            low: i64_to_price(bid_low.value(row)),
            close: i64_to_price(bid_close.value(row)),
        });
        let ask = (!ask_close.is_null(row)).then(|| Bar {
            open: i64_to_price(ask_open.value(row)),
            high: i64_to_price(ask_high.value(row)),
            low: i64_to_price(ask_low.value(row)),
            close: i64_to_price(ask_close.value(row)),
        });
        out.push(QuoteBar {
            symbol: symbol_for_row(template, value.value(row), symbol_sid),
            time: bar_time,
            end_time: bar_end,
            bid,
            ask,
            last_bid_size: i64_to_price(bid_size.value(row)),
            last_ask_size: i64_to_price(ask_size.value(row)),
            period: TimeSpan::from_nanos(period.value(row)),
        });
    }
    Ok(())
}

fn append_ticks(
    batch: &RecordBatch,
    template: &Symbol,
    params: &QueryParams,
    out: &mut Vec<Tick>,
) -> LeanResult<()> {
    let time = int64_named(batch, "time_ns")?;
    let sid = sid_named(batch, "symbol_sid")?;
    let value = string_named(batch, "symbol_value")?;
    let tick_type = tick_type_named(batch, "tick_type")?;
    let tick_value = int64_named(batch, "value")?;
    let quantity = int64_named(batch, "quantity")?;
    let bid_price = int64_named(batch, "bid_price")?;
    let ask_price = int64_named(batch, "ask_price")?;
    let bid_size = int64_named(batch, "bid_size")?;
    let ask_size = int64_named(batch, "ask_size")?;
    let exchange_idx = column_index(batch, "exchange")?;
    let sale_idx = column_index(batch, "sale_condition")?;
    let suspicious = bool_named(batch, "suspicious")?;
    for row in 0..batch.num_rows() {
        let symbol_sid = sid.value(row);
        let tick_time = NanosecondTimestamp(time.value(row));
        if !matches_sid(symbol_sid, params) || !matches_time(tick_time, params) {
            continue;
        }
        out.push(Tick {
            symbol: symbol_for_row(template, value.value(row), symbol_sid),
            time: tick_time,
            tick_type: match tick_type.value(row) {
                1 => TickType::Quote,
                2 => TickType::OpenInterest,
                _ => TickType::Trade,
            },
            value: i64_to_price(tick_value.value(row)),
            quantity: i64_to_price(quantity.value(row)),
            bid_price: i64_to_price(bid_price.value(row)),
            ask_price: i64_to_price(ask_price.value(row)),
            bid_size: i64_to_price(bid_size.value(row)),
            ask_size: i64_to_price(ask_size.value(row)),
            exchange: string_cell(batch.column(exchange_idx).as_ref(), row),
            sale_condition: string_cell(batch.column(sale_idx).as_ref(), row),
            suspicious: suspicious.value(row),
        });
    }
    Ok(())
}

fn append_option_eod(
    batch: &RecordBatch,
    wanted: &HashSet<String>,
    grouped: &mut HashMap<String, Vec<OptionEodBar>>,
) -> LeanResult<()> {
    let date = int64_named(batch, "date_ns")?;
    let symbol_value = string_named(batch, "symbol_value")?;
    let underlying = string_named(batch, "underlying")?;
    let expiration = int64_named(batch, "expiration_ns")?;
    let strike = int64_named(batch, "strike")?;
    let right = string_named(batch, "right")?;
    let open = int64_named(batch, "open")?;
    let high = int64_named(batch, "high")?;
    let low = int64_named(batch, "low")?;
    let close = int64_named(batch, "close")?;
    let volume = int64_named(batch, "volume")?;
    let bid = int64_named(batch, "bid")?;
    let ask = int64_named(batch, "ask")?;
    let bid_size = int64_named(batch, "bid_size")?;
    let ask_size = int64_named(batch, "ask_size")?;
    for row in 0..batch.num_rows() {
        let key = underlying.value(row).to_ascii_uppercase();
        if !wanted.is_empty() && !wanted.contains(&key) {
            continue;
        }
        grouped.entry(key).or_default().push(OptionEodBar {
            date: ns_to_date(date.value(row)),
            symbol_value: symbol_value.value(row).to_string(),
            underlying: underlying.value(row).to_string(),
            expiration: ns_to_date(expiration.value(row)),
            strike: i64_to_price(strike.value(row)),
            right: right.value(row).to_string(),
            open: i64_to_price(open.value(row)),
            high: i64_to_price(high.value(row)),
            low: i64_to_price(low.value(row)),
            close: i64_to_price(close.value(row)),
            volume: volume.value(row),
            bid: i64_to_price(bid.value(row)),
            ask: i64_to_price(ask.value(row)),
            bid_size: bid_size.value(row),
            ask_size: ask_size.value(row),
        });
    }
    Ok(())
}

fn append_factor_rows(batch: &RecordBatch, out: &mut Vec<FactorFileEntry>) -> LeanResult<()> {
    let price = int_or_float64_named(batch, "price_factor")?;
    let split = int_or_float64_named(batch, "split_factor")?;
    let reference = int_or_float64_named(batch, "reference_price")?;
    if let Ok(date_ns) = int64_named(batch, "date_ns") {
        for row in 0..batch.num_rows() {
            out.push(FactorFileEntry {
                date: ns_to_date(date_ns.value(row)),
                price_factor: price.value(row),
                split_factor: split.value(row),
                reference_price: reference.value(row),
            });
        }
    } else {
        let dates = string_named(batch, "date")?;
        for row in 0..batch.num_rows() {
            let date = NaiveDate::parse_from_str(dates.value(row), "%Y-%m-%d")
                .map_err(|e| LeanError::DataError(e.to_string()))?;
            out.push(FactorFileEntry {
                date,
                price_factor: price.value(row),
                split_factor: split.value(row),
                reference_price: reference.value(row),
            });
        }
    }
    Ok(())
}

fn append_map_rows(batch: &RecordBatch, out: &mut Vec<MapFileEntry>) -> LeanResult<()> {
    let ticker = string_named(batch, "ticker")?;
    if let Ok(date_ns) = int64_named(batch, "date_ns") {
        for row in 0..batch.num_rows() {
            out.push(MapFileEntry {
                date: ns_to_date(date_ns.value(row)),
                ticker: ticker.value(row).to_string(),
            });
        }
    } else {
        let dates = string_named(batch, "date")?;
        for row in 0..batch.num_rows() {
            let date = NaiveDate::parse_from_str(dates.value(row), "%Y-%m-%d")
                .map_err(|e| LeanError::DataError(e.to_string()))?;
            out.push(MapFileEntry {
                date,
                ticker: ticker.value(row).to_string(),
            });
        }
    }
    Ok(())
}

fn symbol_for_row(template: &Symbol, symbol_value: &str, symbol_sid: u64) -> Symbol {
    if template.id.sid == symbol_sid {
        return template.clone();
    }
    let symbol = match template.security_type() {
        SecurityType::Equity | SecurityType::Index => {
            Symbol::create_equity(symbol_value, template.market())
        }
        SecurityType::Forex => Symbol::create_forex(symbol_value),
        SecurityType::Crypto => Symbol::create_crypto(symbol_value, template.market()),
        SecurityType::CryptoFuture => Symbol::create_crypto_future(symbol_value, template.market()),
        _ => template.with_value(symbol_value),
    };
    symbol.with_sid(symbol_sid)
}

fn matches_sid(sid: u64, params: &QueryParams) -> bool {
    params
        .predicate
        .symbol_sids
        .as_ref()
        .map(|sids| sids.contains(&sid))
        .unwrap_or(true)
}

fn matches_time(time: DateTime, params: &QueryParams) -> bool {
    params
        .predicate
        .start_time
        .map(|start| time >= start)
        .unwrap_or(true)
        && params
            .predicate
            .end_time
            .map(|end| time <= end)
            .unwrap_or(true)
}

fn matches_bar_time(time: DateTime, params: &QueryParams) -> bool {
    params
        .predicate
        .start_bar_time
        .map(|start| time >= start)
        .unwrap_or(true)
        && params
            .predicate
            .end_bar_time
            .map(|end| time <= end)
            .unwrap_or(true)
}

fn limit<T>(mut rows: Vec<T>, params: &QueryParams) -> Vec<T> {
    if let Some(limit) = params.limit {
        rows.truncate(limit);
    }
    rows
}

fn column_index(batch: &RecordBatch, name: &str) -> LeanResult<usize> {
    batch
        .schema()
        .index_of(name)
        .map_err(|_| LeanError::DataError(format!("{name} column missing")))
}

fn int64_named<'a>(batch: &'a RecordBatch, name: &str) -> LeanResult<&'a Int64Array> {
    batch
        .column(column_index(batch, name)?)
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| LeanError::DataError(format!("{name} column is not Int64")))
}

fn sid_named<'a>(batch: &'a RecordBatch, name: &str) -> LeanResult<SidColumn<'a>> {
    let col = batch.column(column_index(batch, name)?);
    if let Some(values) = col.as_any().downcast_ref::<UInt64Array>() {
        Ok(SidColumn::U64(values))
    } else if let Some(values) = col.as_any().downcast_ref::<Int64Array>() {
        Ok(SidColumn::I64(values))
    } else {
        Err(LeanError::DataError(format!(
            "{name} column is not Int64/UInt64"
        )))
    }
}

enum SidColumn<'a> {
    I64(&'a Int64Array),
    U64(&'a UInt64Array),
}

impl SidColumn<'_> {
    fn value(&self, row: usize) -> u64 {
        match self {
            Self::I64(values) => values.value(row) as u64,
            Self::U64(values) => values.value(row),
        }
    }
}

fn string_named<'a>(batch: &'a RecordBatch, name: &str) -> LeanResult<&'a StringArray> {
    batch
        .column(column_index(batch, name)?)
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| LeanError::DataError(format!("{name} column is not Utf8")))
}

fn bool_named<'a>(batch: &'a RecordBatch, name: &str) -> LeanResult<&'a BooleanArray> {
    batch
        .column(column_index(batch, name)?)
        .as_any()
        .downcast_ref::<BooleanArray>()
        .ok_or_else(|| LeanError::DataError(format!("{name} column is not Boolean")))
}

fn tick_type_named<'a>(batch: &'a RecordBatch, name: &str) -> LeanResult<TickTypeColumn<'a>> {
    let col = batch.column(column_index(batch, name)?);
    if let Some(values) = col.as_any().downcast_ref::<UInt8Array>() {
        Ok(TickTypeColumn::U8(values))
    } else if let Some(values) = col.as_any().downcast_ref::<Int32Array>() {
        Ok(TickTypeColumn::I32(values))
    } else {
        Err(LeanError::DataError(format!(
            "{name} column is not UInt8/Int32"
        )))
    }
}

enum TickTypeColumn<'a> {
    U8(&'a UInt8Array),
    I32(&'a Int32Array),
}

impl TickTypeColumn<'_> {
    fn value(&self, row: usize) -> i32 {
        match self {
            Self::U8(values) => values.value(row) as i32,
            Self::I32(values) => values.value(row),
        }
    }
}

fn int_or_float64_named<'a>(batch: &'a RecordBatch, name: &str) -> LeanResult<NumericColumn<'a>> {
    let col = batch.column(column_index(batch, name)?);
    if let Some(values) = col.as_any().downcast_ref::<arrow_array::Float64Array>() {
        Ok(NumericColumn::F64(values))
    } else if let Some(values) = col.as_any().downcast_ref::<Int64Array>() {
        Ok(NumericColumn::I64(values))
    } else {
        Err(LeanError::DataError(format!(
            "{name} column is not Float64/Int64"
        )))
    }
}

enum NumericColumn<'a> {
    F64(&'a arrow_array::Float64Array),
    I64(&'a Int64Array),
}

impl NumericColumn<'_> {
    fn value(&self, row: usize) -> f64 {
        match self {
            Self::F64(values) => values.value(row),
            Self::I64(values) => values.value(row) as f64,
        }
    }
}

fn string_cell(array: &dyn Array, row: usize) -> Option<String> {
    if array.is_null(row) {
        return None;
    }
    array
        .as_any()
        .downcast_ref::<StringArray>()
        .map(|values| values.value(row).to_string())
}
