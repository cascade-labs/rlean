use rlean_core::{DateTime, Price, Symbol, TimeSpan};
use rlean_data_tables::{BaseData, BaseDataType};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitType {
    /// Warning emitted the trading day before the split takes effect.
    Warning,
    /// The actual split event on the effective date.
    SplitOccurred,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Split {
    pub symbol: Symbol,
    pub time: DateTime,
    /// LEAN split factor, expressed as the price scale applied on the split.
    /// A 2:1 forward split is 0.5; a 1:10 reverse split is 10.
    pub split_factor: Price,
    pub reference_price: Price,
    pub split_type: SplitType,
}

impl Split {
    pub fn new(
        symbol: Symbol,
        time: DateTime,
        split_factor: Price,
        reference_price: Price,
        split_type: SplitType,
    ) -> Self {
        Split {
            symbol,
            time,
            split_factor,
            reference_price,
            split_type,
        }
    }

    pub fn is_warning(&self) -> bool {
        self.split_type == SplitType::Warning
    }
}

impl BaseData for Split {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::Split
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
        self.split_factor
    }
    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}
