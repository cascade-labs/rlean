use crate::time::{NanosecondTimestamp, TimeSpan};
use chrono::{Datelike, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A single session: open and close times as offsets from midnight (nanos).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSession {
    pub open: TimeSpan,
    pub close: TimeSpan,
}

impl MarketSession {
    pub fn new(open_hour: u8, open_min: u8, close_hour: u8, close_min: u8) -> Self {
        MarketSession {
            open: TimeSpan::from_secs(open_hour as i64 * 3600 + open_min as i64 * 60),
            close: TimeSpan::from_secs(close_hour as i64 * 3600 + close_min as i64 * 60),
        }
    }
}

/// Per-weekday sessions. A day with no sessions is a market holiday.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaySchedule {
    pub sessions: Vec<MarketSession>,
}

impl DaySchedule {
    pub fn open(session: MarketSession) -> Self {
        DaySchedule {
            sessions: vec![session],
        }
    }
    pub fn closed() -> Self {
        DaySchedule { sessions: vec![] }
    }
    pub fn is_open(&self) -> bool {
        !self.sessions.is_empty()
    }
}

/// Full exchange hours definition. Mirrors LEAN's `SecurityExchangeHours`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeHours {
    pub timezone: String,
    pub schedule: [DaySchedule; 7], // 0 = Sunday
    pub holidays: HashSet<NaiveDate>,
    pub early_closes: std::collections::HashMap<NaiveDate, TimeSpan>,
    pub late_opens: std::collections::HashMap<NaiveDate, TimeSpan>,
}

impl ExchangeHours {
    pub fn us_equity() -> Self {
        let regular = MarketSession::new(9, 30, 16, 0);
        ExchangeHours {
            timezone: "America/New_York".into(),
            schedule: [
                DaySchedule::closed(),      // Sunday
                DaySchedule::open(regular), // Monday
                DaySchedule::open(regular), // Tuesday
                DaySchedule::open(regular), // Wednesday
                DaySchedule::open(regular), // Thursday
                DaySchedule::open(regular), // Friday
                DaySchedule::closed(),      // Saturday
            ],
            holidays: Self::us_equity_holidays(),
            early_closes: std::collections::HashMap::new(),
            late_opens: std::collections::HashMap::new(),
        }
    }

    pub fn forex_24h() -> Self {
        let session = MarketSession::new(0, 0, 23, 59);
        ExchangeHours {
            timezone: "UTC".into(),
            schedule: [
                DaySchedule::closed(), // Sunday (forex opens Sunday 5pm ET)
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::closed(), // Saturday
            ],
            holidays: HashSet::new(),
            early_closes: std::collections::HashMap::new(),
            late_opens: std::collections::HashMap::new(),
        }
    }

    pub fn crypto_24_7() -> Self {
        let session = MarketSession::new(0, 0, 23, 59);
        ExchangeHours {
            timezone: "UTC".into(),
            schedule: [
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
            ],
            holidays: HashSet::new(),
            early_closes: std::collections::HashMap::new(),
            late_opens: std::collections::HashMap::new(),
        }
    }

    pub fn is_open_at(&self, ts: NanosecondTimestamp) -> bool {
        let tz: Tz = self.timezone.parse().unwrap_or(chrono_tz::UTC);
        let local = ts.to_tz(tz);
        let date = local.date_naive();

        if self.holidays.contains(&date) {
            return false;
        }

        let dow = local.weekday().num_days_from_sunday() as usize;
        let schedule = &self.schedule[dow];

        if !schedule.is_open() {
            return false;
        }

        let secs_since_midnight =
            local.hour() as i64 * 3600 + local.minute() as i64 * 60 + local.second() as i64;
        let day_nanos = secs_since_midnight * 1_000_000_000;

        // Check early close override
        let close_override = self.early_closes.get(&date);
        let open_override = self.late_opens.get(&date);

        schedule.sessions.iter().any(|s| {
            let open = open_override.map(|o| o.nanos).unwrap_or(s.open.nanos);
            let close = close_override.map(|c| c.nanos).unwrap_or(s.close.nanos);
            day_nanos >= open && day_nanos < close
        })
    }

    pub fn next_open(&self, from: NanosecondTimestamp) -> Option<NanosecondTimestamp> {
        // Search up to 10 days ahead
        let tz: Tz = self.timezone.parse().unwrap_or(chrono_tz::UTC);
        let start = from.to_tz(tz);

        for day_offset in 0i64..10 {
            let candidate_date = (start + chrono::Duration::days(day_offset)).date_naive();
            if self.holidays.contains(&candidate_date) {
                continue;
            }
            let dow = candidate_date.weekday().num_days_from_sunday() as usize;
            let schedule = &self.schedule[dow];
            if let Some(session) = schedule.sessions.first() {
                let open_nanos = session.open.nanos;
                let local_dt = tz
                    .from_local_datetime(&candidate_date.and_hms_opt(0, 0, 0).unwrap())
                    .unwrap();
                let utc_dt: chrono::DateTime<chrono::Utc> = local_dt.with_timezone(&chrono::Utc);
                let candidate =
                    NanosecondTimestamp(NanosecondTimestamp::from(utc_dt).0 + open_nanos);
                if candidate > from {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn us_equity_holidays() -> HashSet<NaiveDate> {
        use chrono::NaiveDate;
        let mut h = HashSet::new();
        // 2021 NYSE holidays
        h.insert(NaiveDate::from_ymd_opt(2021, 1, 1).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 1, 18).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 2, 15).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 4, 2).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 5, 31).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 6, 18).unwrap()); // Juneteenth observed
        h.insert(NaiveDate::from_ymd_opt(2021, 7, 5).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 9, 6).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 11, 25).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2021, 12, 24).unwrap());
        // 2022 NYSE holidays
        h.insert(NaiveDate::from_ymd_opt(2022, 1, 17).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 2, 21).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 4, 15).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 5, 30).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 6, 20).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 7, 4).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 9, 5).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 11, 24).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2022, 12, 26).unwrap());
        // 2023 NYSE holidays
        h.insert(NaiveDate::from_ymd_opt(2023, 1, 2).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 1, 16).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 2, 20).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 4, 7).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 5, 29).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 6, 19).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 7, 4).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 9, 4).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 11, 23).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2023, 12, 25).unwrap());
        // 2024 NYSE holidays
        h.insert(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 2, 19).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 3, 29).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 5, 27).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 6, 19).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 7, 4).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 9, 2).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 11, 28).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2024, 12, 25).unwrap());
        // 2025
        h.insert(NaiveDate::from_ymd_opt(2025, 1, 1).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 1, 9).unwrap()); // Carter funeral
        h.insert(NaiveDate::from_ymd_opt(2025, 1, 20).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 2, 17).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 4, 18).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 5, 26).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 6, 19).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 7, 4).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 9, 1).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 11, 27).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2025, 12, 25).unwrap());
        // 2026 NYSE holidays
        h.insert(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 1, 19).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 2, 16).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 4, 3).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 5, 25).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 6, 19).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 7, 3).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 9, 7).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 11, 26).unwrap());
        h.insert(NaiveDate::from_ymd_opt(2026, 12, 25).unwrap());
        h
    }
}
