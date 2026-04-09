use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol, TimeSpan};
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolChangedEvent {
    pub symbol: Symbol,
    pub time: DateTime,
    pub old_symbol: String,
    pub new_symbol: String,
}

impl SymbolChangedEvent {
    pub fn new(symbol: Symbol, time: DateTime, old_symbol: String, new_symbol: String) -> Self {
        SymbolChangedEvent { symbol, time, old_symbol, new_symbol }
    }
}

impl BaseData for SymbolChangedEvent {
    fn data_type(&self) -> BaseDataType { BaseDataType::SymbolChangedEvent }
    fn symbol(&self) -> &Symbol { &self.symbol }
    fn time(&self) -> DateTime { self.time }
    fn end_time(&self) -> DateTime { self.time + TimeSpan::ONE_DAY }
    fn price(&self) -> Price { dec!(0) }
    fn clone_box(&self) -> Box<dyn BaseData> { Box::new(self.clone()) }
}
