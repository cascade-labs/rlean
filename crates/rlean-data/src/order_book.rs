use crate::base_data::{BaseData, BaseDataType};
use rlean_core::{DateTime, Price, Quantity, Symbol};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBookLevel {
    pub price: Price,
    pub size: Quantity,
    pub count: u32,
}

impl OrderBookLevel {
    pub fn new(price: Price, size: Quantity, count: u32) -> Self {
        Self { price, size, count }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderBook {
    pub symbol: Symbol,
    pub time: DateTime,
    pub bids: Vec<OrderBookLevel>,
    pub asks: Vec<OrderBookLevel>,
}

impl OrderBook {
    pub fn new(
        symbol: Symbol,
        time: DateTime,
        bids: Vec<OrderBookLevel>,
        asks: Vec<OrderBookLevel>,
    ) -> Self {
        Self {
            symbol,
            time,
            bids,
            asks,
        }
    }

    pub fn best_bid(&self) -> Option<&OrderBookLevel> {
        self.bids
            .iter()
            .filter(|level| level.price > dec!(0) && level.size > dec!(0))
            .max_by(|a, b| a.price.cmp(&b.price))
    }

    pub fn best_ask(&self) -> Option<&OrderBookLevel> {
        self.asks
            .iter()
            .filter(|level| level.price > dec!(0) && level.size > dec!(0))
            .min_by(|a, b| a.price.cmp(&b.price))
    }

    pub fn mid_price(&self) -> Price {
        match (self.best_bid(), self.best_ask()) {
            (Some(bid), Some(ask)) => (bid.price + ask.price) / dec!(2),
            (Some(bid), None) => bid.price,
            (None, Some(ask)) => ask.price,
            (None, None) => dec!(0),
        }
    }
}

impl BaseData for OrderBook {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::OrderBook
    }

    fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    fn time(&self) -> DateTime {
        self.time
    }

    fn end_time(&self) -> DateTime {
        self.time
    }

    fn price(&self) -> Price {
        self.mid_price()
    }

    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}
