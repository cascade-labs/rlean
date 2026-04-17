use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Quantity, Symbol, TimeSpan};
use rust_decimal::Decimal;
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

    /// Parse from LEAN forex CSV:
    /// `ms,bid_open*10000,bid_high*10000,bid_low*10000,bid_close*10000,bid_volume,
    ///    ask_open*10000,ask_high*10000,ask_low*10000,ask_close*10000,ask_volume`
    pub fn from_lean_csv_line(
        line: &str,
        symbol: Symbol,
        date: chrono::NaiveDate,
        period: TimeSpan,
    ) -> Option<Self> {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 10 {
            return None;
        }

        let ms: i64 = parts[0].trim().parse().ok()?;
        let scale = dec!(100000); // pip scale for forex

        let parse =
            |s: &str| -> Option<Decimal> { s.trim().parse::<Decimal>().ok().map(|v| v / scale) };

        let bid = Bar::new(
            parse(parts[1])?,
            parse(parts[2])?,
            parse(parts[3])?,
            parse(parts[4])?,
        );
        let last_bid_size: Quantity = parts[5].trim().parse().unwrap_or(dec!(0));

        let ask = Bar::new(
            parse(parts[6])?,
            parse(parts[7])?,
            parse(parts[8])?,
            parse(parts[9])?,
        );
        let last_ask_size: Quantity = parts
            .get(10)
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(dec!(0));

        use chrono::{TimeZone, Utc};
        let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
        let bar_nanos = midnight.timestamp() * 1_000_000_000 + ms * 1_000_000;
        let time = lean_core::NanosecondTimestamp(bar_nanos);

        Some(QuoteBar::new(
            symbol,
            time,
            period,
            Some(bid),
            Some(ask),
            last_bid_size,
            last_ask_size,
        ))
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
