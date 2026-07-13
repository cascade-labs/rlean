use chrono::{Datelike, NaiveDate};

pub enum DateRule {
    EveryDay,
    EveryWeekDay,
    Every(Vec<chrono::Weekday>),
    MonthStart { days_offset: i64 },
    MonthEnd { days_offset: i64 },
    On(Vec<NaiveDate>),
}

pub struct DateRules;

impl DateRules {
    pub fn every_day() -> DateRule {
        DateRule::EveryDay
    }
    pub fn every_week_day() -> DateRule {
        DateRule::EveryWeekDay
    }
    pub fn every(weekdays: Vec<chrono::Weekday>) -> DateRule {
        DateRule::Every(weekdays)
    }
    pub fn month_start() -> DateRule {
        DateRule::MonthStart { days_offset: 0 }
    }
    pub fn month_end() -> DateRule {
        DateRule::MonthEnd { days_offset: 0 }
    }
    pub fn on(dates: Vec<NaiveDate>) -> DateRule {
        DateRule::On(dates)
    }
}

impl DateRule {
    pub fn applies_on(&self, date: NaiveDate) -> bool {
        match self {
            DateRule::EveryDay => true,
            DateRule::EveryWeekDay => {
                !matches!(date.weekday(), chrono::Weekday::Sat | chrono::Weekday::Sun)
            }
            DateRule::Every(days) => days.contains(&date.weekday()),
            DateRule::MonthStart { days_offset } => date.day() as i64 == 1 + days_offset,
            DateRule::MonthEnd { days_offset } => {
                let next_month = if date.month() == 12 {
                    NaiveDate::from_ymd_opt(date.year() + 1, 1, 1)
                } else {
                    NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1)
                };
                next_month
                    .map(|nm| {
                        let last = nm - chrono::Duration::days(1);
                        let target = last - chrono::Duration::days((-*days_offset) as u64 as i64);
                        date == target
                    })
                    .unwrap_or(false)
            }
            DateRule::On(dates) => dates.contains(&date),
        }
    }
}
