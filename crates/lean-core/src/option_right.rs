use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString, FromRepr};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    Display,
    EnumString,
    EnumIter,
    FromRepr,
)]
#[repr(u8)]
pub enum OptionRight {
    #[strum(serialize = "Call")]
    Call = 0,
    #[strum(serialize = "Put")]
    Put = 1,
}

impl OptionRight {
    pub fn is_call(&self) -> bool {
        matches!(self, OptionRight::Call)
    }
    pub fn is_put(&self) -> bool {
        matches!(self, OptionRight::Put)
    }
    pub fn opposite(&self) -> Self {
        match self {
            OptionRight::Call => OptionRight::Put,
            OptionRight::Put => OptionRight::Call,
        }
    }
}
