use chrono::{Datelike, TimeZone, Timelike, Utc};
use lean_core::time::{NanosecondTimestamp, TimeSpan};

#[test]
fn from_millis_and_back() {
    let ts = NanosecondTimestamp::from_millis(1_000);
    assert_eq!(ts.as_millis(), 1_000);
    assert_eq!(ts.as_secs(), 1);
}

#[test]
fn from_secs_round_trip() {
    let ts = NanosecondTimestamp::from_secs(86400);
    assert_eq!(ts.as_secs(), 86400);
}

#[test]
fn timestamp_ordering() {
    let t1 = NanosecondTimestamp::from_secs(100);
    let t2 = NanosecondTimestamp::from_secs(200);
    assert!(t1 < t2);
    assert!(t2 > t1);
    assert_eq!(t1, NanosecondTimestamp::from_secs(100));
}

#[test]
fn add_timespan() {
    let t = NanosecondTimestamp::from_secs(1000);
    let result = t + TimeSpan::from_secs(60);
    assert_eq!(result.as_secs(), 1060);
}

#[test]
fn sub_timespan() {
    let t = NanosecondTimestamp::from_secs(1000);
    let result = t - TimeSpan::from_secs(100);
    assert_eq!(result.as_secs(), 900);
}

#[test]
fn sub_timestamps_gives_timespan() {
    let t2 = NanosecondTimestamp::from_secs(300);
    let t1 = NanosecondTimestamp::from_secs(100);
    let span = t2 - t1;
    assert_eq!(span.nanos, 200_000_000_000);
    assert_eq!(span.total_seconds(), 200.0);
}

#[test]
fn from_chrono_datetime() {
    let dt = Utc.with_ymd_and_hms(2024, 1, 15, 9, 30, 0).unwrap();
    let ts = NanosecondTimestamp::from(dt);
    let back = ts.to_utc();
    assert_eq!(back.date_naive(), dt.date_naive());
    assert_eq!(back.time(), dt.time());
}

#[test]
fn timespan_one_day_constants() {
    assert_eq!(TimeSpan::ONE_DAY.nanos, 86_400_000_000_000);
    assert_eq!(TimeSpan::ONE_HOUR.nanos, 3_600_000_000_000);
    assert_eq!(TimeSpan::ONE_MINUTE.nanos, 60_000_000_000);
    assert_eq!(TimeSpan::ONE_SECOND.nanos, 1_000_000_000);
}

#[test]
fn timespan_total_conversions() {
    let day = TimeSpan::ONE_DAY;
    assert!((day.total_days() - 1.0).abs() < 1e-10);
    assert!((day.total_hours() - 24.0).abs() < 1e-10);
    assert!((day.total_minutes() - 1440.0).abs() < 1e-10);
}

#[test]
fn timezone_conversion() {
    let ts = NanosecondTimestamp::from(Utc.with_ymd_and_hms(2024, 1, 15, 14, 30, 0).unwrap());
    // UTC 14:30 = New York 09:30 (EST, UTC-5)
    let ny = ts.to_tz(lean_core::time::tz::NEW_YORK);
    assert_eq!(ny.time().hour(), 9);
    assert_eq!(ny.time().minute(), 30);
}

#[test]
fn date_from_timestamp() {
    let ts = NanosecondTimestamp::from(Utc.with_ymd_and_hms(2024, 3, 15, 12, 0, 0).unwrap());
    let date = ts.date_utc();
    assert_eq!(date.year(), 2024);
    assert_eq!(date.month(), 3);
    assert_eq!(date.day(), 15);
}
