use serde::{Deserialize, Serialize};
use strum::{Display, EnumIter, EnumString};

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
    Default,
)]
pub enum DataNormalizationMode {
    /// Raw, unadjusted prices.
    Raw,
    /// Adjusted for splits and dividends (backward-adjusted from current price).
    #[default]
    Adjusted,
    /// Split-adjusted only.
    SplitAdjusted,
    /// Dividend-adjusted only.
    TotalReturn,
    /// Forward-adjusted (all historical prices projected forward to current splits/dividends).
    ForwardPanamaCanal,
    /// Backward-adjusted Panama Canal method.
    BackwardPanamaCanal,
}
