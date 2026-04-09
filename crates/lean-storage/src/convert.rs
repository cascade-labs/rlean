use crate::schema::{date_to_ns, i64_to_price, ns_to_date, price_to_i64, OptionEodBar, OptionUniverseRow, PRICE_SCALE};
use arrow_array::*;
use arrow_array::builder::*;
use arrow_schema::Schema;
use lean_data::{QuoteBar, Tick, TradeBar};
use lean_core::TickType;
use std::sync::Arc;

// ─── TradeBar ────────────────────────────────────────────────────────────────

pub fn trade_bars_to_record_batch(bars: &[TradeBar]) -> arrow_array::RecordBatch {
    let n = bars.len();
    let schema = crate::schema::trade_bar_schema();

    let time_ns:     Vec<i64> = bars.iter().map(|b| b.time.0).collect();
    let end_time_ns: Vec<i64> = bars.iter().map(|b| b.end_time.0).collect();
    let sym_sid:     Vec<u64> = bars.iter().map(|b| b.symbol.id.sid).collect();
    let sym_val:     Vec<&str> = bars.iter().map(|b| b.symbol.value.as_str()).collect();
    let open:        Vec<i64> = bars.iter().map(|b| price_to_i64(&b.open)).collect();
    let high:        Vec<i64> = bars.iter().map(|b| price_to_i64(&b.high)).collect();
    let low:         Vec<i64> = bars.iter().map(|b| price_to_i64(&b.low)).collect();
    let close:       Vec<i64> = bars.iter().map(|b| price_to_i64(&b.close)).collect();
    let volume:      Vec<i64> = bars.iter().map(|b| price_to_i64(&b.volume)).collect();
    let period_ns:   Vec<i64> = bars.iter().map(|b| b.period.nanos).collect();

    arrow_array::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(time_ns)),
            Arc::new(Int64Array::from(end_time_ns)),
            Arc::new(UInt64Array::from(sym_sid)),
            Arc::new(StringArray::from(sym_val)),
            Arc::new(Int64Array::from(open)),
            Arc::new(Int64Array::from(high)),
            Arc::new(Int64Array::from(low)),
            Arc::new(Int64Array::from(close)),
            Arc::new(Int64Array::from(volume)),
            Arc::new(Int64Array::from(period_ns)),
        ],
    )
    .expect("schema/column count mismatch — this is a bug")
}

pub fn record_batch_to_trade_bars(batch: &arrow_array::RecordBatch, symbol: lean_core::Symbol) -> Vec<TradeBar> {
    let n = batch.num_rows();
    if n == 0 { return vec![]; }

    let time_ns  = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let end_ns   = batch.column(1).as_any().downcast_ref::<Int64Array>().unwrap();
    let open_col = batch.column(4).as_any().downcast_ref::<Int64Array>().unwrap();
    let high_col = batch.column(5).as_any().downcast_ref::<Int64Array>().unwrap();
    let low_col  = batch.column(6).as_any().downcast_ref::<Int64Array>().unwrap();
    let clos_col = batch.column(7).as_any().downcast_ref::<Int64Array>().unwrap();
    let vol_col  = batch.column(8).as_any().downcast_ref::<Int64Array>().unwrap();
    let per_col  = batch.column(9).as_any().downcast_ref::<Int64Array>().unwrap();

    (0..n).map(|i| {
        let time = lean_core::NanosecondTimestamp(time_ns.value(i));
        let period = lean_core::TimeSpan::from_nanos(per_col.value(i));
        TradeBar {
            symbol: symbol.clone(),
            time,
            end_time: lean_core::NanosecondTimestamp(end_ns.value(i)),
            open: i64_to_price(open_col.value(i)),
            high: i64_to_price(high_col.value(i)),
            low:  i64_to_price(low_col.value(i)),
            close: i64_to_price(clos_col.value(i)),
            volume: i64_to_price(vol_col.value(i)),
            period,
        }
    }).collect()
}

// ─── Tick ─────────────────────────────────────────────────────────────────────

pub fn ticks_to_record_batch(ticks: &[Tick]) -> arrow_array::RecordBatch {
    let schema = crate::schema::tick_schema();
    let n = ticks.len();

    let time_ns:   Vec<i64>   = ticks.iter().map(|t| t.time.0).collect();
    let sym_sid:   Vec<u64>   = ticks.iter().map(|t| t.symbol.id.sid).collect();
    let sym_val:   Vec<&str>  = ticks.iter().map(|t| t.symbol.value.as_str()).collect();
    let tick_type: Vec<u8>    = ticks.iter().map(|t| t.tick_type as u8).collect();
    let value:     Vec<i64>   = ticks.iter().map(|t| price_to_i64(&t.value)).collect();
    let quantity:  Vec<i64>   = ticks.iter().map(|t| price_to_i64(&t.quantity)).collect();
    let bid_price: Vec<i64>   = ticks.iter().map(|t| price_to_i64(&t.bid_price)).collect();
    let ask_price: Vec<i64>   = ticks.iter().map(|t| price_to_i64(&t.ask_price)).collect();
    let bid_size:  Vec<i64>   = ticks.iter().map(|t| price_to_i64(&t.bid_size)).collect();
    let ask_size:  Vec<i64>   = ticks.iter().map(|t| price_to_i64(&t.ask_size)).collect();
    let exchange:  Vec<Option<&str>> = ticks.iter().map(|t| t.exchange.as_deref()).collect();
    let sale_cond: Vec<Option<&str>> = ticks.iter().map(|t| t.sale_condition.as_deref()).collect();
    let suspicious: Vec<bool> = ticks.iter().map(|t| t.suspicious).collect();

    arrow_array::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(time_ns)),
            Arc::new(UInt64Array::from(sym_sid)),
            Arc::new(StringArray::from(sym_val)),
            Arc::new(UInt8Array::from(tick_type)),
            Arc::new(Int64Array::from(value)),
            Arc::new(Int64Array::from(quantity)),
            Arc::new(Int64Array::from(bid_price)),
            Arc::new(Int64Array::from(ask_price)),
            Arc::new(Int64Array::from(bid_size)),
            Arc::new(Int64Array::from(ask_size)),
            Arc::new(StringArray::from(exchange)),
            Arc::new(StringArray::from(sale_cond)),
            Arc::new(BooleanArray::from(suspicious)),
        ],
    )
    .expect("schema/column count mismatch — this is a bug")
}

// ─── QuoteBar ─────────────────────────────────────────────────────────────────

pub fn quote_bars_to_record_batch(bars: &[QuoteBar]) -> arrow_array::RecordBatch {
    let schema = crate::schema::quote_bar_schema();
    let n = bars.len();

    macro_rules! opt_price_col {
        ($field:expr) => {
            bars.iter().map(|b| $field.map(|p| price_to_i64(&p))).collect::<Vec<_>>()
        };
    }

    let time_ns:     Vec<i64>         = bars.iter().map(|b| b.time.0).collect();
    let end_time_ns: Vec<i64>         = bars.iter().map(|b| b.end_time.0).collect();
    let sym_sid:     Vec<u64>         = bars.iter().map(|b| b.symbol.id.sid).collect();
    let sym_val:     Vec<&str>        = bars.iter().map(|b| b.symbol.value.as_str()).collect();
    let bid_open:    Vec<Option<i64>> = bars.iter().map(|b| b.bid.as_ref().map(|bar| price_to_i64(&bar.open))).collect();
    let bid_high:    Vec<Option<i64>> = bars.iter().map(|b| b.bid.as_ref().map(|bar| price_to_i64(&bar.high))).collect();
    let bid_low:     Vec<Option<i64>> = bars.iter().map(|b| b.bid.as_ref().map(|bar| price_to_i64(&bar.low))).collect();
    let bid_close:   Vec<Option<i64>> = bars.iter().map(|b| b.bid.as_ref().map(|bar| price_to_i64(&bar.close))).collect();
    let ask_open:    Vec<Option<i64>> = bars.iter().map(|b| b.ask.as_ref().map(|bar| price_to_i64(&bar.open))).collect();
    let ask_high:    Vec<Option<i64>> = bars.iter().map(|b| b.ask.as_ref().map(|bar| price_to_i64(&bar.high))).collect();
    let ask_low:     Vec<Option<i64>> = bars.iter().map(|b| b.ask.as_ref().map(|bar| price_to_i64(&bar.low))).collect();
    let ask_close:   Vec<Option<i64>> = bars.iter().map(|b| b.ask.as_ref().map(|bar| price_to_i64(&bar.close))).collect();
    let lbs:         Vec<i64>         = bars.iter().map(|b| price_to_i64(&b.last_bid_size)).collect();
    let las:         Vec<i64>         = bars.iter().map(|b| price_to_i64(&b.last_ask_size)).collect();
    let period_ns:   Vec<i64>         = bars.iter().map(|b| b.period.nanos).collect();

    arrow_array::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(time_ns)),
            Arc::new(Int64Array::from(end_time_ns)),
            Arc::new(UInt64Array::from(sym_sid)),
            Arc::new(StringArray::from(sym_val)),
            Arc::new(Int64Array::from(bid_open)),
            Arc::new(Int64Array::from(bid_high)),
            Arc::new(Int64Array::from(bid_low)),
            Arc::new(Int64Array::from(bid_close)),
            Arc::new(Int64Array::from(ask_open)),
            Arc::new(Int64Array::from(ask_high)),
            Arc::new(Int64Array::from(ask_low)),
            Arc::new(Int64Array::from(ask_close)),
            Arc::new(Int64Array::from(lbs)),
            Arc::new(Int64Array::from(las)),
            Arc::new(Int64Array::from(period_ns)),
        ],
    )
    .expect("schema/column count mismatch — this is a bug")
}

// ─── OptionEodBar ─────────────────────────────────────────────────────────────

pub fn option_eod_bars_to_record_batch(bars: &[OptionEodBar]) -> arrow_array::RecordBatch {
    let schema = crate::schema::option_eod_bar_schema();

    let date_ns_col:   Vec<i64>  = bars.iter().map(|b| date_to_ns(b.date)).collect();
    let symbol_value:  Vec<&str> = bars.iter().map(|b| b.symbol_value.as_str()).collect();
    let underlying:    Vec<&str> = bars.iter().map(|b| b.underlying.as_str()).collect();
    let expiration_ns: Vec<i64>  = bars.iter().map(|b| date_to_ns(b.expiration)).collect();
    let strike:        Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.strike)).collect();
    let right:         Vec<&str> = bars.iter().map(|b| b.right.as_str()).collect();
    let open:          Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.open)).collect();
    let high:          Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.high)).collect();
    let low:           Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.low)).collect();
    let close:         Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.close)).collect();
    let volume:        Vec<i64>  = bars.iter().map(|b| b.volume).collect();
    let bid:           Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.bid)).collect();
    let ask:           Vec<i64>  = bars.iter().map(|b| price_to_i64(&b.ask)).collect();
    let bid_size:      Vec<i64>  = bars.iter().map(|b| b.bid_size).collect();
    let ask_size:      Vec<i64>  = bars.iter().map(|b| b.ask_size).collect();

    arrow_array::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(date_ns_col)),
            Arc::new(StringArray::from(symbol_value)),
            Arc::new(StringArray::from(underlying)),
            Arc::new(Int64Array::from(expiration_ns)),
            Arc::new(Int64Array::from(strike)),
            Arc::new(StringArray::from(right)),
            Arc::new(Int64Array::from(open)),
            Arc::new(Int64Array::from(high)),
            Arc::new(Int64Array::from(low)),
            Arc::new(Int64Array::from(close)),
            Arc::new(Int64Array::from(volume)),
            Arc::new(Int64Array::from(bid)),
            Arc::new(Int64Array::from(ask)),
            Arc::new(Int64Array::from(bid_size)),
            Arc::new(Int64Array::from(ask_size)),
        ],
    )
    .expect("schema/column count mismatch — this is a bug")
}

pub fn record_batch_to_option_eod_bars(batch: &arrow_array::RecordBatch) -> Vec<OptionEodBar> {
    let n = batch.num_rows();
    if n == 0 { return vec![]; }

    let date_ns_col    = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let sym_val_col    = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    let underlying_col = batch.column(2).as_any().downcast_ref::<StringArray>().unwrap();
    let exp_ns_col     = batch.column(3).as_any().downcast_ref::<Int64Array>().unwrap();
    let strike_col     = batch.column(4).as_any().downcast_ref::<Int64Array>().unwrap();
    let right_col      = batch.column(5).as_any().downcast_ref::<StringArray>().unwrap();
    let open_col       = batch.column(6).as_any().downcast_ref::<Int64Array>().unwrap();
    let high_col       = batch.column(7).as_any().downcast_ref::<Int64Array>().unwrap();
    let low_col        = batch.column(8).as_any().downcast_ref::<Int64Array>().unwrap();
    let close_col      = batch.column(9).as_any().downcast_ref::<Int64Array>().unwrap();
    let volume_col     = batch.column(10).as_any().downcast_ref::<Int64Array>().unwrap();
    let bid_col        = batch.column(11).as_any().downcast_ref::<Int64Array>().unwrap();
    let ask_col        = batch.column(12).as_any().downcast_ref::<Int64Array>().unwrap();
    let bid_sz_col     = batch.column(13).as_any().downcast_ref::<Int64Array>().unwrap();
    let ask_sz_col     = batch.column(14).as_any().downcast_ref::<Int64Array>().unwrap();

    (0..n).map(|i| OptionEodBar {
        date:         ns_to_date(date_ns_col.value(i)),
        symbol_value: sym_val_col.value(i).to_string(),
        underlying:   underlying_col.value(i).to_string(),
        expiration:   ns_to_date(exp_ns_col.value(i)),
        strike:       i64_to_price(strike_col.value(i)),
        right:        right_col.value(i).to_string(),
        open:         i64_to_price(open_col.value(i)),
        high:         i64_to_price(high_col.value(i)),
        low:          i64_to_price(low_col.value(i)),
        close:        i64_to_price(close_col.value(i)),
        volume:       volume_col.value(i),
        bid:          i64_to_price(bid_col.value(i)),
        ask:          i64_to_price(ask_col.value(i)),
        bid_size:     bid_sz_col.value(i),
        ask_size:     ask_sz_col.value(i),
    }).collect()
}

// ─── OptionUniverseRow ────────────────────────────────────────────────────────

pub fn option_universe_rows_to_record_batch(rows: &[OptionUniverseRow]) -> arrow_array::RecordBatch {
    let schema = crate::schema::option_universe_schema();

    let date_ns_col:   Vec<i64>  = rows.iter().map(|r| date_to_ns(r.date)).collect();
    let symbol_value:  Vec<&str> = rows.iter().map(|r| r.symbol_value.as_str()).collect();
    let underlying:    Vec<&str> = rows.iter().map(|r| r.underlying.as_str()).collect();
    let expiration_ns: Vec<i64>  = rows.iter().map(|r| date_to_ns(r.expiration)).collect();
    let strike:        Vec<i64>  = rows.iter().map(|r| price_to_i64(&r.strike)).collect();
    let right:         Vec<&str> = rows.iter().map(|r| r.right.as_str()).collect();

    arrow_array::RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(date_ns_col)),
            Arc::new(StringArray::from(symbol_value)),
            Arc::new(StringArray::from(underlying)),
            Arc::new(Int64Array::from(expiration_ns)),
            Arc::new(Int64Array::from(strike)),
            Arc::new(StringArray::from(right)),
        ],
    )
    .expect("schema/column count mismatch — this is a bug")
}

pub fn record_batch_to_option_universe_rows(batch: &arrow_array::RecordBatch) -> Vec<OptionUniverseRow> {
    let n = batch.num_rows();
    if n == 0 { return vec![]; }

    let date_ns_col    = batch.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let sym_val_col    = batch.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    let underlying_col = batch.column(2).as_any().downcast_ref::<StringArray>().unwrap();
    let exp_ns_col     = batch.column(3).as_any().downcast_ref::<Int64Array>().unwrap();
    let strike_col     = batch.column(4).as_any().downcast_ref::<Int64Array>().unwrap();
    let right_col      = batch.column(5).as_any().downcast_ref::<StringArray>().unwrap();

    (0..n).map(|i| OptionUniverseRow {
        date:         ns_to_date(date_ns_col.value(i)),
        symbol_value: sym_val_col.value(i).to_string(),
        underlying:   underlying_col.value(i).to_string(),
        expiration:   ns_to_date(exp_ns_col.value(i)),
        strike:       i64_to_price(strike_col.value(i)),
        right:        right_col.value(i).to_string(),
    }).collect()
}
