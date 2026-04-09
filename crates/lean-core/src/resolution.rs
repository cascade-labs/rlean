use serde::{Deserialize, Serialize};
use std::time::Duration;
use strum::{Display, EnumIter, EnumString, FromRepr};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
    Serialize, Deserialize, Display, EnumString, EnumIter, FromRepr,
)]
#[repr(u8)]
pub enum Resolution {
    #[strum(serialize = "Tick")]
    Tick = 0,
    #[strum(serialize = "Second")]
    Second = 1,
    #[strum(serialize = "Minute")]
    Minute = 2,
    #[strum(serialize = "Hour")]
    Hour = 3,
    #[strum(serialize = "Daily")]
    Daily = 4,
}

impl Resolution {
    /// Duration of a single bar at this resolution.
    /// Returns None for Tick (variable duration).
    pub fn to_duration(self) -> Option<Duration> {
        match self {
            Resolution::Tick => None,
            Resolution::Second => Some(Duration::from_secs(1)),
            Resolution::Minute => Some(Duration::from_secs(60)),
            Resolution::Hour => Some(Duration::from_secs(3600)),
            Resolution::Daily => Some(Duration::from_secs(86400)),
        }
    }

    /// Duration in nanoseconds for non-tick resolutions.
    pub fn to_nanos(self) -> Option<u64> {
        self.to_duration().map(|d| d.as_nanos() as u64)
    }

    pub fn is_tick(&self) -> bool {
        matches!(self, Resolution::Tick)
    }

    /// Lower resolutions use daily files; higher use per-date files.
    pub fn is_high_resolution(&self) -> bool {
        matches!(self, Resolution::Tick | Resolution::Second | Resolution::Minute)
    }

    /// LEAN's canonical data folder name for this resolution.
    pub fn folder_name(&self) -> &'static str {
        match self {
            Resolution::Tick => "tick",
            Resolution::Second => "second",
            Resolution::Minute => "minute",
            Resolution::Hour => "hour",
            Resolution::Daily => "daily",
        }
    }
}

impl Default for Resolution {
    fn default() -> Self {
        Resolution::Daily
    }
}
