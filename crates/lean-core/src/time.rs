use chrono::{DateTime as ChronoDateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};

/// Nanosecond-precision UTC timestamp — primary time representation.
/// Stored as i64 nanos since Unix epoch (covers ±292 years).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NanosecondTimestamp(pub i64);

impl NanosecondTimestamp {
    pub const EPOCH: Self = NanosecondTimestamp(0);
    pub const MIN: Self = NanosecondTimestamp(i64::MIN);
    pub const MAX: Self = NanosecondTimestamp(i64::MAX);

    pub fn now() -> Self {
        let ts = Utc::now();
        NanosecondTimestamp(ts.timestamp_nanos_opt().unwrap_or(0))
    }

    pub fn from_millis(ms: i64) -> Self {
        NanosecondTimestamp(ms * 1_000_000)
    }

    pub fn from_micros(us: i64) -> Self {
        NanosecondTimestamp(us * 1_000)
    }

    pub fn from_secs(s: i64) -> Self {
        NanosecondTimestamp(s * 1_000_000_000)
    }

    pub fn as_millis(&self) -> i64 {
        self.0 / 1_000_000
    }

    pub fn as_micros(&self) -> i64 {
        self.0 / 1_000
    }

    pub fn as_secs(&self) -> i64 {
        self.0 / 1_000_000_000
    }

    pub fn to_utc(&self) -> ChronoDateTime<Utc> {
        let secs = self.0 / 1_000_000_000;
        let nanos = (self.0 % 1_000_000_000) as u32;
        Utc.timestamp_opt(secs, nanos).unwrap()
    }

    pub fn to_tz(&self, tz: Tz) -> ChronoDateTime<Tz> {
        self.to_utc().with_timezone(&tz)
    }

    pub fn date_utc(&self) -> NaiveDate {
        self.to_utc().date_naive()
    }
}

impl From<ChronoDateTime<Utc>> for NanosecondTimestamp {
    fn from(dt: ChronoDateTime<Utc>) -> Self {
        NanosecondTimestamp(dt.timestamp_nanos_opt().unwrap_or(0))
    }
}

impl From<NaiveDateTime> for NanosecondTimestamp {
    fn from(ndt: NaiveDateTime) -> Self {
        let dt = Utc.from_utc_datetime(&ndt);
        NanosecondTimestamp::from(dt)
    }
}

impl Add<TimeSpan> for NanosecondTimestamp {
    type Output = Self;
    fn add(self, rhs: TimeSpan) -> Self {
        NanosecondTimestamp(self.0 + rhs.nanos)
    }
}

impl Sub<TimeSpan> for NanosecondTimestamp {
    type Output = Self;
    fn sub(self, rhs: TimeSpan) -> Self {
        NanosecondTimestamp(self.0 - rhs.nanos)
    }
}

impl Sub<NanosecondTimestamp> for NanosecondTimestamp {
    type Output = TimeSpan;
    fn sub(self, rhs: NanosecondTimestamp) -> TimeSpan {
        TimeSpan { nanos: self.0 - rhs.0 }
    }
}

impl std::fmt::Display for NanosecondTimestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.to_utc().format("%Y-%m-%dT%H:%M:%S%.9fZ"))
    }
}

/// Duration in nanoseconds — mirrors C# TimeSpan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimeSpan {
    pub nanos: i64,
}

impl TimeSpan {
    pub const ZERO: Self = TimeSpan { nanos: 0 };
    pub const ONE_SECOND: Self = TimeSpan { nanos: 1_000_000_000 };
    pub const ONE_MINUTE: Self = TimeSpan { nanos: 60_000_000_000 };
    pub const ONE_HOUR: Self = TimeSpan { nanos: 3_600_000_000_000 };
    pub const ONE_DAY: Self = TimeSpan { nanos: 86_400_000_000_000 };

    pub fn from_nanos(n: i64) -> Self { TimeSpan { nanos: n } }
    pub fn from_micros(us: i64) -> Self { TimeSpan { nanos: us * 1_000 } }
    pub fn from_millis(ms: i64) -> Self { TimeSpan { nanos: ms * 1_000_000 } }
    pub fn from_secs(s: i64) -> Self { TimeSpan { nanos: s * 1_000_000_000 } }
    pub fn from_mins(m: i64) -> Self { TimeSpan::from_secs(m * 60) }
    pub fn from_hours(h: i64) -> Self { TimeSpan::from_secs(h * 3600) }
    pub fn from_days(d: i64) -> Self { TimeSpan::from_secs(d * 86400) }

    pub fn total_seconds(&self) -> f64 { self.nanos as f64 / 1e9 }
    pub fn total_minutes(&self) -> f64 { self.nanos as f64 / 60e9 }
    pub fn total_hours(&self) -> f64 { self.nanos as f64 / 3600e9 }
    pub fn total_days(&self) -> f64 { self.nanos as f64 / 86400e9 }

    pub fn as_chrono_duration(&self) -> Duration {
        Duration::nanoseconds(self.nanos)
    }
}

impl Add for TimeSpan {
    type Output = Self;
    fn add(self, rhs: Self) -> Self { TimeSpan { nanos: self.nanos + rhs.nanos } }
}

impl Sub for TimeSpan {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self { TimeSpan { nanos: self.nanos - rhs.nanos } }
}

impl std::fmt::Display for TimeSpan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total_secs = self.nanos.abs() / 1_000_000_000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        let ns = self.nanos.abs() % 1_000_000_000;
        if self.nanos < 0 { write!(f, "-")?; }
        write!(f, "{:02}:{:02}:{:02}", h, m, s)?;
        if ns > 0 { write!(f, ".{:09}", ns)?; }
        Ok(())
    }
}

/// Alias used throughout the engine for datetime values (UTC).
pub type DateTime = NanosecondTimestamp;

/// Common US market timezone constants.
pub mod tz {
    use chrono_tz::Tz;

    pub const NEW_YORK: Tz = chrono_tz::America::New_York;
    pub const CHICAGO: Tz = chrono_tz::America::Chicago;
    pub const LOS_ANGELES: Tz = chrono_tz::America::Los_Angeles;
    pub const LONDON: Tz = chrono_tz::Europe::London;
    pub const TOKYO: Tz = chrono_tz::Asia::Tokyo;
    pub const HONG_KONG: Tz = chrono_tz::Asia::Hong_Kong;
    pub const UTC: Tz = chrono_tz::UTC;
}
