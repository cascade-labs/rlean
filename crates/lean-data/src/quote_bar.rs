use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Quantity, Symbol, TimeSpan};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// OHLC bar for a single side (bid or ask).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bar {
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
}

impl Bar {
    pub fn new(open: Price, high: Price, low: Price, close: Price) -> Self {
        Bar {
            open,
            high,
            low,
            close,
        }
    }

    pub fn from_price(price: Price) -> Self {
        Bar {
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

/// Bid/ask OHLC bar for forex and options.
/// Stores separate bid and ask bars plus last-known sizes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteBar {
    pub symbol: Symbol,
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
        QuoteBar {
            symbol,
            time,
            end_time: time + period,
            bid,
            ask,
            last_bid_size,
            last_ask_size,
            period,
        }
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
