use arrow_schema::{DataType, Field, Schema};
use rlean_core::{DateTime, Price, Quantity, Symbol, TimeSpan};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{decimal_type, BaseData, BaseDataType, PartitionField, TableContract};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeBarData {
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Quantity,
}

impl TradeBarData {
    pub fn new(open: Price, high: Price, low: Price, close: Price, volume: Quantity) -> Self {
        Self {
            open,
            high,
            low,
            close,
            volume,
        }
    }
}

/// LEAN-compatible OHLCV bar and the canonical `market_trade_bars` value type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeBar {
    pub symbol: Symbol,
    /// Physical execution/data venue. This is distinct from the LEAN market
    /// encoded in `symbol` and participates in persisted row identity.
    #[serde(default)]
    pub venue: Option<String>,
    pub time: DateTime,
    pub end_time: DateTime,
    pub open: Price,
    pub high: Price,
    pub low: Price,
    pub close: Price,
    pub volume: Quantity,
    pub period: TimeSpan,
}

impl TradeBar {
    pub fn new(symbol: Symbol, time: DateTime, period: TimeSpan, data: TradeBarData) -> Self {
        Self {
            symbol,
            venue: None,
            time,
            end_time: time + period,
            open: data.open,
            high: data.high,
            low: data.low,
            close: data.close,
            volume: data.volume,
            period,
        }
    }

    pub fn with_venue(mut self, venue: impl Into<String>) -> Self {
        self.venue = Some(venue.into());
        self
    }

    pub fn spread_pct(&self) -> Decimal {
        if self.close.is_zero() {
            dec!(0)
        } else {
            (self.high - self.low) / self.close
        }
    }

    pub fn true_range(&self) -> Decimal {
        self.high - self.low
    }

    pub fn is_valid(&self) -> bool {
        self.open > dec!(0)
            && self.high >= self.open
            && self.high >= self.close
            && self.low <= self.open
            && self.low <= self.close
            && self.low > dec!(0)
    }

    pub fn update(&mut self, price: Price, volume: Quantity) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        self.volume += volume;
    }

    pub fn merge(&mut self, other: &TradeBar) {
        if other.high > self.high {
            self.high = other.high;
        }
        if other.low < self.low {
            self.low = other.low;
        }
        self.close = other.close;
        self.volume += other.volume;
        self.end_time = other.end_time;
        self.period = TimeSpan::from_nanos(self.end_time.0 - self.time.0);
    }
}

impl std::fmt::Display for TradeBar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} O:{} H:{} L:{} C:{} V:{}",
            self.symbol, self.open, self.high, self.low, self.close, self.volume
        )
    }
}

impl BaseData for TradeBar {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::TradeBar
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
        self.close
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}

impl TableContract for TradeBar {
    const TABLE_NAME: &'static str = "market_trade_bars";
    const PARTITION_FIELDS: &'static [PartitionField] = &[
        PartitionField::identity("security_type"),
        PartitionField::identity("market"),
        PartitionField::identity("resolution"),
        PartitionField::month("day"),
    ];

    fn schema() -> Arc<Schema> {
        Arc::new(Schema::new(vec![
            Field::new("time_ns", DataType::Int64, false),
            Field::new("end_time_ns", DataType::Int64, false),
            Field::new("symbol_sid", DataType::Int64, false),
            Field::new("symbol_value", DataType::Utf8, false),
            // Nullable until the existing prototype data is backfilled.
            Field::new("venue", DataType::Utf8, true),
            Field::new("open", decimal_type(), false),
            Field::new("high", decimal_type(), false),
            Field::new("low", decimal_type(), false),
            Field::new("close", decimal_type(), false),
            Field::new("volume", decimal_type(), false),
            Field::new("period_ns", DataType::Int64, false),
            Field::new("security_type", DataType::Utf8, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("resolution", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ]))
    }
}
