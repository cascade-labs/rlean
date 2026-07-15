use std::fmt;

use rlean_core::{DateTime, Price, Symbol};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BaseDataType {
    TradeBar,
    QuoteBar,
    Tick,
    OpenInterest,
    Dividend,
    Split,
    Delisting,
    SymbolChangedEvent,
    Fundamental,
    MarginInterestRate,
    OrderBook,
    Custom,
}

/// Common behavior of LEAN-compatible data values.
pub trait BaseData: Send + Sync + fmt::Debug + 'static {
    fn data_type(&self) -> BaseDataType;
    fn symbol(&self) -> &Symbol;
    fn venue(&self) -> Option<&str> {
        None
    }
    fn time(&self) -> DateTime;
    fn end_time(&self) -> DateTime;
    fn price(&self) -> Price;

    fn value(&self) -> Price {
        self.price()
    }

    fn is_live(&self) -> bool {
        false
    }

    fn clone_box(&self) -> Box<dyn BaseData>;
}

impl Clone for Box<dyn BaseData> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
