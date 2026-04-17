use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol, TimeSpan};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenInterest {
    pub symbol: Symbol,
    pub time: DateTime,
    pub value: Price,
}

impl OpenInterest {
    pub fn new(symbol: Symbol, time: DateTime, value: Price) -> Self {
        OpenInterest {
            symbol,
            time,
            value,
        }
    }
}

impl BaseData for OpenInterest {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::OpenInterest
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
        self.value
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}
