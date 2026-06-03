use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol};
use serde::{Deserialize, Serialize};

/// Per-symbol funding or margin interest rate data.
///
/// For crypto futures this mirrors LEAN's `MarginInterestRate`: the rate is
/// delivered as market data and the brokerage margin-interest model decides
/// when to apply it to portfolio cash.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MarginInterestRate {
    pub symbol: Symbol,
    pub time: DateTime,
    pub interest_rate: Price,
}

impl MarginInterestRate {
    pub fn new(symbol: Symbol, time: DateTime, interest_rate: Price) -> Self {
        Self {
            symbol,
            time,
            interest_rate,
        }
    }
}

impl BaseData for MarginInterestRate {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::MarginInterestRate
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
        self.interest_rate
    }

    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}
