use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Quantity, Symbol, TimeSpan};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

/// OHLCV bar. The workhorse of every equity/futures/forex backtest.
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

/// OHLCV bar. The workhorse of every equity/futures/forex backtest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TradeBar {
    pub symbol: Symbol,
    /// Bar open time (UTC nanoseconds)
    pub time: DateTime,
    /// Bar close time = time + period
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
        TradeBar {
            symbol,
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

    /// Intra-bar spread as a fraction of close.
    pub fn spread_pct(&self) -> Decimal {
        if self.close.is_zero() {
            return dec!(0);
        }
        (self.high - self.low) / self.close
    }

    /// True range (same as ATR numerator, no previous close).
    pub fn true_range(&self) -> Decimal {
        self.high - self.low
    }

    /// Returns true if this bar has valid (positive) OHLC.
    pub fn is_valid(&self) -> bool {
        self.open > dec!(0)
            && self.high >= self.open
            && self.high >= self.close
            && self.low <= self.open
            && self.low <= self.close
            && self.low > dec!(0)
    }

    /// Update bar with a new trade price and volume. Used when aggregating ticks.
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

    /// Consolidate another bar into this one (extend end_time).
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

impl BaseData for TradeBar {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::TradeBar
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
        self.close
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
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
