use chrono::{NaiveDate, Weekday};
use rlean_core::{Market, MarketHoursDatabase, Symbol, TimeSpan};
use rlean_scheduling::date_rules::{DateRule, DateRules};
use rlean_scheduling::time_rules::{TimeRule, TimeRules};

fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(year, month, day).unwrap()
}

#[test]
fn date_rules_weekdays_month_boundaries_and_on_dates_match_lean_basics() {
    // Mirrors LEAN DateRulesTests for no-symbol calendar rules.
    assert!(DateRules::every_day().applies_on(date(2024, 1, 6)));
    assert!(!DateRules::every_week_day().applies_on(date(2024, 1, 6)));
    assert!(DateRules::every_week_day().applies_on(date(2024, 1, 8)));
    assert!(DateRules::every(vec![Weekday::Mon]).applies_on(date(2024, 1, 8)));
    assert!(!DateRules::every(vec![Weekday::Mon]).applies_on(date(2024, 1, 9)));
    assert!(DateRules::month_start().applies_on(date(2024, 1, 1)));
    assert!(DateRules::month_end().applies_on(date(2024, 1, 31)));
    assert!(DateRules::on(vec![date(2024, 2, 29)]).applies_on(date(2024, 2, 29)));
}

#[test]
fn date_rule_offsets_match_lean_month_start_and_end_offsets() {
    assert!(DateRule::MonthStart { days_offset: 5 }.applies_on(date(2024, 1, 6)));
    assert!(!DateRule::MonthStart { days_offset: 5 }.applies_on(date(2024, 1, 5)));
    assert!(DateRule::MonthEnd { days_offset: -2 }.applies_on(date(2024, 1, 29)));
    assert!(!DateRule::MonthEnd { days_offset: -2 }.applies_on(date(2024, 1, 30)));
}

#[test]
fn time_rules_construct_expected_offsets() {
    // Mirrors LEAN TimeRulesTests for At/Every/market-offset construction.
    match TimeRules::at(9, 31) {
        TimeRule::At { hour, minute } => assert_eq!((hour, minute), (9, 31)),
        _ => panic!("expected At rule"),
    }
    match TimeRules::after_market_open(15) {
        TimeRule::AfterMarketOpen { minutes_after_open } => {
            assert_eq!(minutes_after_open, 15)
        }
        _ => panic!("expected AfterMarketOpen rule"),
    }
    let spy = Symbol::create_equity("SPY", &Market::usa());
    match TimeRules::before_market_close(spy.clone(), 10) {
        TimeRule::BeforeMarketClose {
            symbol,
            minutes_before_close,
            extended_market_close,
        } => {
            assert_eq!(symbol, spy);
            assert_eq!(minutes_before_close, 10);
            assert!(!extended_market_close);
        }
        _ => panic!("expected BeforeMarketClose rule"),
    }
    match TimeRules::every(30) {
        TimeRule::Every(span) => assert_eq!(span, TimeSpan::from_mins(30)),
        _ => panic!("expected Every rule"),
    }
}

#[test]
fn before_market_close_uses_exchange_calendar_and_early_close() {
    use std::sync::{Arc, Mutex};

    let spy = Symbol::create_equity("SPY", &Market::usa());
    let fired = Arc::new(Mutex::new(0usize));
    let count = fired.clone();
    let manager = rlean_scheduling::ScheduleManager::new();
    manager.add(
        "rebalance",
        DateRule::EveryDay,
        TimeRules::before_market_close(spy, 15),
        move || {
            *count.lock().unwrap() += 1;
            Ok(())
        },
    );

    // 2025-07-03 is a 13:00 America/New_York close, so the event is 16:45 UTC.
    let start = chrono::DateTime::parse_from_rfc3339("2025-07-03T16:44:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
        .into();
    let due = chrono::DateTime::parse_from_rfc3339("2025-07-03T16:45:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc)
        .into();
    manager.skip_until(start);
    let events = manager.due_events(due, MarketHoursDatabase::global().as_ref());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].trigger_time, due);
    (events[0].callback.lock())().unwrap();
    assert_eq!(*fired.lock().unwrap(), 1);
}

#[test]
fn time_rules_every_rejects_non_positive_intervals_like_lean() {
    // Mirrors LEAN TimeRulesTests: Every throws for zero or negative intervals.
    assert!(std::panic::catch_unwind(|| TimeRules::every(0)).is_err());
    assert!(std::panic::catch_unwind(|| TimeRules::every(-1)).is_err());
}
