use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
         Display, EnumString, EnumIter)]
pub enum TickType {
    Trade,
    Quote,
    OpenInterest,
}
