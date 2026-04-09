use lean_core::Resolution;
use std::time::Duration;

#[test]
fn tick_has_no_duration() {
    assert!(Resolution::Tick.to_duration().is_none());
    assert!(Resolution::Tick.is_tick());
    assert!(!Resolution::Daily.is_tick());
}

#[test]
fn second_resolution_is_one_second() {
    assert_eq!(Resolution::Second.to_duration(), Some(Duration::from_secs(1)));
}

#[test]
fn minute_resolution_is_60_seconds() {
    assert_eq!(Resolution::Minute.to_duration(), Some(Duration::from_secs(60)));
}

#[test]
fn hour_resolution_is_3600_seconds() {
    assert_eq!(Resolution::Hour.to_duration(), Some(Duration::from_secs(3600)));
}

#[test]
fn daily_resolution_is_86400_seconds() {
    assert_eq!(Resolution::Daily.to_duration(), Some(Duration::from_secs(86400)));
}

#[test]
fn high_resolution_flag() {
    assert!(Resolution::Tick.is_high_resolution());
    assert!(Resolution::Second.is_high_resolution());
    assert!(Resolution::Minute.is_high_resolution());
    assert!(!Resolution::Hour.is_high_resolution());
    assert!(!Resolution::Daily.is_high_resolution());
}

#[test]
fn folder_names() {
    assert_eq!(Resolution::Tick.folder_name(), "tick");
    assert_eq!(Resolution::Second.folder_name(), "second");
    assert_eq!(Resolution::Minute.folder_name(), "minute");
    assert_eq!(Resolution::Hour.folder_name(), "hour");
    assert_eq!(Resolution::Daily.folder_name(), "daily");
}

#[test]
fn resolution_ordering() {
    assert!(Resolution::Tick < Resolution::Second);
    assert!(Resolution::Second < Resolution::Minute);
    assert!(Resolution::Minute < Resolution::Hour);
    assert!(Resolution::Hour < Resolution::Daily);
}
