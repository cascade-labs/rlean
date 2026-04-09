use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString, FromRepr};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize,
         Display, EnumString, EnumIter, FromRepr)]
#[repr(u8)]
pub enum OptionStyle {
    #[strum(serialize = "American")]
    American = 0,
    #[strum(serialize = "European")]
    European = 1,
}
