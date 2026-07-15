use arrow_schema::{DataType, Field, Schema};
use rlean_core::{DateTime, Price, Quantity, Symbol, TickType};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, BaseData, BaseDataType, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    pub symbol: Symbol,
    /// Dataset/execution venue. `exchange` remains the per-tick exchange code
    /// supplied by the upstream feed.
    #[serde(default)]
    pub venue: Option<String>,
    pub time: DateTime,
    pub tick_type: TickType,
    pub value: Price,
    pub quantity: Quantity,
    pub bid_price: Price,
    pub ask_price: Price,
    pub bid_size: Quantity,
    pub ask_size: Quantity,
    pub exchange: Option<String>,
    pub sale_condition: Option<String>,
    pub suspicious: bool,
}

impl Tick {
    pub fn trade(symbol: Symbol, time: DateTime, price: Price, quantity: Quantity) -> Self {
        Self {
            symbol,
            venue: None,
            time,
            tick_type: TickType::Trade,
            value: price,
            quantity,
            bid_price: dec!(0),
            ask_price: dec!(0),
            bid_size: dec!(0),
            ask_size: dec!(0),
            exchange: None,
            sale_condition: None,
            suspicious: false,
        }
    }
    pub fn quote(
        symbol: Symbol,
        time: DateTime,
        bid: Price,
        ask: Price,
        bid_size: Quantity,
        ask_size: Quantity,
    ) -> Self {
        let value = if bid > dec!(0) && ask > dec!(0) {
            (bid + ask) / dec!(2)
        } else if bid > dec!(0) {
            bid
        } else {
            ask
        };
        Self {
            symbol,
            venue: None,
            time,
            tick_type: TickType::Quote,
            value,
            quantity: dec!(0),
            bid_price: bid,
            ask_price: ask,
            bid_size,
            ask_size,
            exchange: None,
            sale_condition: None,
            suspicious: false,
        }
    }
    pub fn open_interest(symbol: Symbol, time: DateTime, oi: Quantity) -> Self {
        Self {
            symbol,
            venue: None,
            time,
            tick_type: TickType::OpenInterest,
            value: oi,
            quantity: oi,
            bid_price: dec!(0),
            ask_price: dec!(0),
            bid_size: dec!(0),
            ask_size: dec!(0),
            exchange: None,
            sale_condition: None,
            suspicious: false,
        }
    }
    pub fn with_venue(mut self, venue: impl Into<String>) -> Self {
        self.venue = Some(venue.into());
        self
    }
    pub fn spread(&self) -> Price {
        if self.ask_price > dec!(0) && self.bid_price > dec!(0) {
            self.ask_price - self.bid_price
        } else {
            dec!(0)
        }
    }
    pub fn is_trade(&self) -> bool {
        self.tick_type == TickType::Trade
    }
    pub fn is_quote(&self) -> bool {
        self.tick_type == TickType::Quote
    }
}

impl BaseData for Tick {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::Tick
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
        self.time
    }
    fn price(&self) -> Price {
        self.value
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}

impl TableContract for Tick {
    const TABLE_NAME: &'static str = "market_ticks";
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
            Field::new("symbol_sid", DataType::Int64, false),
            Field::new("symbol_value", DataType::Utf8, false),
            // Nullable until the existing prototype data is backfilled.
            Field::new("venue", DataType::Utf8, true),
            Field::new("tick_type", DataType::UInt8, false),
            Field::new("value", d.clone(), false),
            Field::new("quantity", d.clone(), false),
            Field::new("bid_price", d.clone(), false),
            Field::new("ask_price", d.clone(), false),
            Field::new("bid_size", d.clone(), false),
            Field::new("ask_size", d, false),
            Field::new("exchange", DataType::Utf8, true),
            Field::new("sale_condition", DataType::Utf8, true),
            Field::new("suspicious", DataType::Boolean, false),
            Field::new("security_type", DataType::Utf8, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("resolution", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
