use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString, FromRepr};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
    Display, EnumString, EnumIter, FromRepr,
)]
#[repr(u8)]
pub enum SecurityType {
    #[strum(serialize = "Base")]
    Base = 0,
    #[strum(serialize = "Equity")]
    Equity = 1,
    #[strum(serialize = "Option")]
    Option = 2,
    #[strum(serialize = "Commodity")]
    Commodity = 3,
    #[strum(serialize = "Forex")]
    Forex = 4,
    #[strum(serialize = "Future")]
    Future = 5,
    #[strum(serialize = "Cfd")]
    Cfd = 6,
    #[strum(serialize = "Crypto")]
    Crypto = 7,
    #[strum(serialize = "FutureOption")]
    FutureOption = 8,
    #[strum(serialize = "IndexOption")]
    IndexOption = 9,
    #[strum(serialize = "Index")]
    Index = 10,
    #[strum(serialize = "CryptoFuture")]
    CryptoFuture = 11,
}

impl SecurityType {
    pub fn is_option_like(&self) -> bool {
        matches!(self, SecurityType::Option | SecurityType::FutureOption | SecurityType::IndexOption)
    }

    pub fn is_future_like(&self) -> bool {
        matches!(self, SecurityType::Future | SecurityType::FutureOption | SecurityType::CryptoFuture)
    }
}

impl Default for SecurityType {
    fn default() -> Self {
        SecurityType::Equity
    }
}
