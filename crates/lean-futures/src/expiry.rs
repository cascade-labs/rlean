use chrono::{Datelike, Duration, NaiveDate, Weekday};

/// How a futures contract's expiry date is determined.
#[derive(Debug, Clone, Copy)]
pub enum ExpiryRule {
    /// 3rd Friday of expiry month (e.g., equity index futures)
    ThirdFriday,
    /// Last business day of expiry month
    LastBusinessDay,
    /// Last trading day N business days before month end
    NthFromEnd(i32),
    /// Specific day-of-month (e.g., crude oil = 20th - 3 business days)
    MonthlyFixed(u32),
    /// Last Thursday (e.g., Eurodollar)
    LastThursday,
}

/// Returns the expiry date for a given expiry rule, year, and month.
pub fn compute_expiry(rule: ExpiryRule, year: i32, month: u32) -> NaiveDate {
    match rule {
        ExpiryRule::ThirdFriday => third_friday(year, month),
        ExpiryRule::LastBusinessDay => last_business_day(year, month),
        ExpiryRule::NthFromEnd(n) => nth_business_day_from_end(year, month, n),
        ExpiryRule::MonthlyFixed(day) => {
            NaiveDate::from_ymd_opt(year, month, day)
                .unwrap_or_else(|| last_business_day(year, month))
        }
        ExpiryRule::LastThursday => last_weekday(year, month, Weekday::Thu),
    }
}

fn third_friday(year: i32, month: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let days_to_friday = (Weekday::Fri.num_days_from_monday() as i32
        - first.weekday().num_days_from_monday() as i32)
        .rem_euclid(7);
    first + Duration::days((days_to_friday + 14) as i64)
}

pub(crate) fn last_business_day(year: i32, month: u32) -> NaiveDate {
    let next_month = if month == 12 {
        NaiveDate::from_ymd_opt(year + 1, 1, 1)
    } else {
        NaiveDate::from_ymd_opt(year, month + 1, 1)
    };
    let mut d = next_month.unwrap() - Duration::days(1);
    while d.weekday() == Weekday::Sat || d.weekday() == Weekday::Sun {
        d -= Duration::days(1);
    }
    d
}

fn nth_business_day_from_end(year: i32, month: u32, n: i32) -> NaiveDate {
    let mut d = last_business_day(year, month);
    let mut count = 0;
    while count < n {
        d -= Duration::days(1);
        if d.weekday() != Weekday::Sat && d.weekday() != Weekday::Sun {
            count += 1;
        }
    }
    d
}

fn last_weekday(year: i32, month: u32, weekday: Weekday) -> NaiveDate {
    let mut d = last_business_day(year, month);
    while d.weekday() != weekday {
        d -= Duration::days(1);
    }
    d
}
