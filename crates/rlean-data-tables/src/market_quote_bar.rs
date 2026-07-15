use arrow_schema::{DataType, Field, Schema};
use rlean_core::{DateTime, Price, Quantity, Symbol, TimeSpan};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, BaseData, BaseDataType, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
}

impl Bar {
    pub fn new(open: Price, high: Price, low: Price, close: Price) -> Self {
        Self {
            open,
            high,
            low,
            close,
        }
    }
    pub fn from_price(price: Price) -> Self {
        Self {
            open: price,
            high: price,
            low: price,
            close: price,
        }
    }
    pub fn update(&mut self, price: Price) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
    }
    pub fn merge(&mut self, other: &Bar) {
        if other.high > self.high {
            self.high = other.high;
        }
        if other.low < self.low {
            self.low = other.low;
        }
        self.close = other.close;
    }
}

/// LEAN-compatible bid/ask bar and the canonical `market_quote_bars` value type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteBar {
    pub symbol: Symbol,
    /// Physical execution/data venue, separate from the symbol's LEAN market.
    #[serde(default)]
    pub venue: Option<String>,
    pub time: DateTime,
    pub end_time: DateTime,
    pub bid: Option<Bar>,
    pub ask: Option<Bar>,
    pub last_bid_size: Quantity,
    pub last_ask_size: Quantity,
    pub period: TimeSpan,
}

impl QuoteBar {
    pub fn new(
        symbol: Symbol,
        time: DateTime,
        period: TimeSpan,
        bid: Option<Bar>,
        ask: Option<Bar>,
        last_bid_size: Quantity,
        last_ask_size: Quantity,
    ) -> Self {
        Self {
            symbol,
            venue: None,
            time,
            end_time: time + period,
            bid,
            ask,
            last_bid_size,
            last_ask_size,
            period,
        }
    }
    pub fn with_venue(mut self, venue: impl Into<String>) -> Self {
        self.venue = Some(venue.into());
        self
    }
    pub fn mid_open(&self) -> Price {
        match (&self.bid, &self.ask) {
            (Some(b), Some(a)) => (b.open + a.open) / dec!(2),
            (Some(b), None) => b.open,
            (None, Some(a)) => a.open,
            _ => dec!(0),
        }
    }
    pub fn mid_close(&self) -> Price {
        match (&self.bid, &self.ask) {
            (Some(b), Some(a)) => (b.close + a.close) / dec!(2),
            (Some(b), None) => b.close,
            (None, Some(a)) => a.close,
            _ => dec!(0),
        }
    }
    pub fn spread(&self) -> Option<Price> {
        match (&self.bid, &self.ask) {
            (Some(b), Some(a)) => Some(a.close - b.close),
            _ => None,
        }
    }
    pub fn update(&mut self, bid: Price, ask: Price, bid_size: Quantity, ask_size: Quantity) {
        if let Some(b) = &mut self.bid {
            b.update(bid);
        }
        if let Some(a) = &mut self.ask {
            a.update(ask);
        }
        self.last_bid_size = bid_size;
        self.last_ask_size = ask_size;
    }
    pub fn merge(&mut self, other: &QuoteBar) {
        if let (Some(b), Some(ob)) = (&mut self.bid, &other.bid) {
            b.merge(ob);
        }
        if let (Some(a), Some(oa)) = (&mut self.ask, &other.ask) {
            a.merge(oa);
        }
        self.last_bid_size = other.last_bid_size;
        self.last_ask_size = other.last_ask_size;
        self.end_time = other.end_time;
        self.period = TimeSpan::from_nanos(self.end_time.0 - self.time.0);
    }
}

impl BaseData for QuoteBar {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::QuoteBar
    }
    fn symbol(&self) -> &Symbol {
        &self.symbol
    }
    fn venue(&self) -> Option<&str> {
        self.venue.as_deref()
    }
    fn time(&self) -> DateTime {
        self.time
    }
    fn end_time(&self) -> DateTime {
        self.end_time
    }
    fn price(&self) -> Price {
        self.mid_close()
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}

impl TableContract for QuoteBar {
    const TABLE_NAME: &'static str = "market_quote_bars";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("security_type"),
        PartitionField::identity("market"),
        PartitionField::identity("resolution"),
        PartitionField::month("day"),
    ];
    fn schema() -> Arc<Schema> {
        let d = decimal_type();
        Arc::new(Schema::new(vec![
            Field::new("time_ns", DataType::Int64, false),
            Field::new("end_time_ns", DataType::Int64, false),
            Field::new("symbol_sid", DataType::Int64, false),
            Field::new("symbol_value", DataType::Utf8, false),
            // Nullable until the existing prototype data is backfilled.
            Field::new("venue", DataType::Utf8, true),
            Field::new("bid_open", d.clone(), true),
            Field::new("bid_high", d.clone(), true),
            Field::new("bid_low", d.clone(), true),
            Field::new("bid_close", d.clone(), true),
            Field::new("ask_open", d.clone(), true),
            Field::new("ask_high", d.clone(), true),
            Field::new("ask_low", d.clone(), true),
            Field::new("ask_close", d.clone(), true),
            Field::new("last_bid_size", d.clone(), false),
            Field::new("last_ask_size", d, false),
            Field::new("period_ns", DataType::Int64, false),
            Field::new("security_type", DataType::Utf8, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("resolution", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
