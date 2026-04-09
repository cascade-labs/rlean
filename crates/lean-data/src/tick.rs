use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Quantity, Symbol, TickType};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// Raw tick data — the most granular market data type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tick {
    pub symbol: Symbol,
    pub time: DateTime,
    pub tick_type: TickType,
    /// Last trade/mid price
    pub value: Price,
    /// Trade volume (for Trade ticks)
    pub quantity: Quantity,
    /// Bid price (for Quote ticks)
    pub bid_price: Price,
    /// Ask price (for Quote ticks)
    pub ask_price: Price,
    /// Bid size (for Quote ticks)
    pub bid_size: Quantity,
    /// Ask size (for Quote ticks)
    pub ask_size: Quantity,
    /// Exchange where trade occurred
    pub exchange: Option<String>,
    /// SIP sale condition flags
    pub sale_condition: Option<String>,
    /// True if tick is suspicious (e.g., outlier)
    pub suspicious: bool,
}

impl Tick {
    pub fn trade(
        symbol: Symbol,
        time: DateTime,
        price: Price,
        quantity: Quantity,
    ) -> Self {
        Tick {
            symbol,
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
        let mid = if bid > dec!(0) && ask > dec!(0) {
            (bid + ask) / dec!(2)
        } else if bid > dec!(0) { bid } else { ask };

        Tick {
            symbol,
            time,
            tick_type: TickType::Quote,
            value: mid,
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
        Tick {
            symbol,
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

    pub fn spread(&self) -> Price {
        if self.ask_price > dec!(0) && self.bid_price > dec!(0) {
            self.ask_price - self.bid_price
        } else {
            dec!(0)
        }
    }

    pub fn is_trade(&self) -> bool { self.tick_type == TickType::Trade }
    pub fn is_quote(&self) -> bool { self.tick_type == TickType::Quote }

    /// Parse from LEAN tick CSV (trade):
    /// `milliseconds,price*10000,quantity`
    pub fn from_lean_trade_csv(line: &str, symbol: Symbol, date: chrono::NaiveDate) -> Option<Self> {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 3 { return None; }

        let ms: i64 = parts[0].trim().parse().ok()?;
        let price = parts[1].trim().parse::<rust_decimal::Decimal>().ok()? / dec!(10000);
        let quantity = parts[2].trim().parse().ok()?;

        use chrono::{TimeZone, Utc};
        let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
        let nanos = midnight.timestamp() * 1_000_000_000 + ms * 1_000_000;

        Some(Tick::trade(symbol, lean_core::NanosecondTimestamp(nanos), price, quantity))
    }

    /// Parse from LEAN tick CSV (quote):
    /// `milliseconds,bid*10000,ask*10000,bid_size,ask_size`
    pub fn from_lean_quote_csv(line: &str, symbol: Symbol, date: chrono::NaiveDate) -> Option<Self> {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 5 { return None; }

        let ms: i64 = parts[0].trim().parse().ok()?;
        let bid = parts[1].trim().parse::<rust_decimal::Decimal>().ok()? / dec!(10000);
        let ask = parts[2].trim().parse::<rust_decimal::Decimal>().ok()? / dec!(10000);
        let bid_size = parts[3].trim().parse().ok()?;
        let ask_size = parts[4].trim().parse().ok()?;

        use chrono::{TimeZone, Utc};
        let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
        let nanos = midnight.timestamp() * 1_000_000_000 + ms * 1_000_000;

        Some(Tick::quote(symbol, lean_core::NanosecondTimestamp(nanos), bid, ask, bid_size, ask_size))
    }
}

impl BaseData for Tick {
    fn data_type(&self) -> BaseDataType { BaseDataType::Tick }
    fn symbol(&self) -> &Symbol { &self.symbol }
    fn time(&self) -> DateTime { self.time }
    fn end_time(&self) -> DateTime { self.time }
    fn price(&self) -> Price { self.value }
    fn clone_box(&self) -> Box<dyn BaseData> { Box::new(self.clone()) }
}
