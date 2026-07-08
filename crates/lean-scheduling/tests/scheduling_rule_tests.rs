use chrono::{NaiveDate, Weekday};
use lean_core::TimeSpan;
use lean_scheduling::date_rules::{DateRule, DateRules};
use lean_scheduling::time_rules::{TimeRule, TimeRules};

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
        TimeRule::At(span) => assert_eq!(span, TimeSpan::from_secs(9 * 3600 + 31 * 60)),
        _ => panic!("expected At rule"),
    }
    match TimeRules::after_market_open(15) {
        TimeRule::AfterMarketOpen { offset } => assert_eq!(offset, TimeSpan::from_mins(15)),
        _ => panic!("expected AfterMarketOpen rule"),
    }
    match TimeRules::before_market_close(10) {
        TimeRule::BeforeMarketClose { offset } => assert_eq!(offset, TimeSpan::from_mins(10)),
        _ => panic!("expected BeforeMarketClose rule"),
    }
    match TimeRules::every(30) {
        TimeRule::Every(span) => assert_eq!(span, TimeSpan::from_mins(30)),
        _ => panic!("expected Every rule"),
    }
}

#[test]
fn time_rules_every_rejects_non_positive_intervals_like_lean() {
    // Mirrors LEAN TimeRulesTests: Every throws for zero or negative intervals.
    assert!(std::panic::catch_unwind(|| TimeRules::every(0)).is_err());
    assert!(std::panic::catch_unwind(|| TimeRules::every(-1)).is_err());
}
