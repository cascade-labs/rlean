use crate::TimeSpan;
use serde::{Deserialize, Serialize};

/// Named period — used by indicators and scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Period {
    pub span: TimeSpan,
}

impl Period {
    pub fn new(span: TimeSpan) -> Self { Period { span } }
    pub fn from_days(d: i64) -> Self { Period::new(TimeSpan::from_days(d)) }
    pub fn from_hours(h: i64) -> Self { Period::new(TimeSpan::from_hours(h)) }
    pub fn from_minutes(m: i64) -> Self { Period::new(TimeSpan::from_mins(m)) }
    pub fn from_seconds(s: i64) -> Self { Period::new(TimeSpan::from_secs(s)) }

    pub fn nanos(&self) -> i64 { self.span.nanos }
}
