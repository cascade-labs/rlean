use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol, TimeSpan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dividend {
    pub symbol: Symbol,
    pub time: DateTime,
    pub distribution: Price,
    pub reference_price: Price,
}

impl Dividend {
    pub fn new(symbol: Symbol, time: DateTime, distribution: Price, reference_price: Price) -> Self {
        Dividend { symbol, time, distribution, reference_price }
    }

    pub fn split_factor(&self) -> Price {
        use rust_decimal_macros::dec;
        if self.reference_price.is_zero() { return dec!(1); }
        (self.reference_price - self.distribution) / self.reference_price
    }
}

impl BaseData for Dividend {
    fn data_type(&self) -> BaseDataType { BaseDataType::Dividend }
    fn symbol(&self) -> &Symbol { &self.symbol }
    fn time(&self) -> DateTime { self.time }
    fn end_time(&self) -> DateTime { self.time + TimeSpan::ONE_DAY }
    fn price(&self) -> Price { self.distribution }
    fn clone_box(&self) -> Box<dyn BaseData> { Box::new(self.clone()) }
}
