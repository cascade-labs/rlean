use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol, TimeSpan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DelistingType {
    Warning,
    Delisted,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Delisting {
    pub symbol: Symbol,
    pub time: DateTime,
    pub price: Price,
    pub delisting_type: DelistingType,
}

impl Delisting {
    pub fn new(
        symbol: Symbol,
        time: DateTime,
        price: Price,
        delisting_type: DelistingType,
    ) -> Self {
        Delisting {
            symbol,
            time,
            price,
            delisting_type,
        }
    }

    pub fn is_warning(&self) -> bool {
        self.delisting_type == DelistingType::Warning
    }
}

impl BaseData for Delisting {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::Delisting
    }
    fn symbol(&self) -> &Symbol {
        &self.symbol
    }
    fn time(&self) -> DateTime {
        self.time
    }
    fn end_time(&self) -> DateTime {
        self.time + TimeSpan::ONE_DAY
    }
    fn price(&self) -> Price {
        self.price
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}
