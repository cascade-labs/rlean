use serde::{Deserialize, Serialize};

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
