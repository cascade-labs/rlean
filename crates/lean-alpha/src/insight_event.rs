use crate::insight::Insight;
use lean_core::DateTime;
use serde::{Deserialize, Serialize};

pub const INSIGHT_EVENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsightEventKind {
    Emitted,
    Expired,
    FlatClosed,
    UniverseRemoved,
    RiskCancelled,
    Restored,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightEvent {
    pub schema_version: u32,
    pub event_time_utc: DateTime,
    pub kind: InsightEventKind,
    pub insight: Insight,
}

impl InsightEvent {
    pub fn new(kind: InsightEventKind, event_time_utc: DateTime, insight: Insight) -> Self {
        Self {
            schema_version: INSIGHT_EVENT_SCHEMA_VERSION,
            event_time_utc,
            kind,
            insight,
        }
    }
}
