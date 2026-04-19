use lean_core::{DateTime, Price, Symbol};
use serde::{Deserialize, Serialize};
use std::fmt;

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
    Custom,
}

/// Timezone information for a data subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataTimeZoneInfo {
    /// Timezone for the data timestamps as stored on disk.
    pub data_tz: String,
    /// Exchange timezone for this symbol.
    pub exchange_tz: String,
}

impl Default for DataTimeZoneInfo {
    fn default() -> Self {
        DataTimeZoneInfo {
            data_tz: "UTC".into(),
            exchange_tz: "America/New_York".into(),
        }
    }
}

/// Core trait for all market data types in the engine.
/// Mirrors LEAN's `BaseData` abstract class.
pub trait BaseData: Send + Sync + fmt::Debug + 'static {
    fn data_type(&self) -> BaseDataType;
    fn symbol(&self) -> &Symbol;
    fn time(&self) -> DateTime;
    fn end_time(&self) -> DateTime;
    fn price(&self) -> Price;
    fn value(&self) -> Price {
        self.price()
    }

    /// True if this data point is "live" (not from historical replay).
    fn is_live(&self) -> bool {
        false
    }

    /// Clone into a boxed trait object.
    fn clone_box(&self) -> Box<dyn BaseData>;
}

impl Clone for Box<dyn BaseData> {
    fn clone(&self) -> Self {
        self.clone_box()
    }
}
