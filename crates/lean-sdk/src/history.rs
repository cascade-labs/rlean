//! SDK-owned history request and result APIs.

use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::{DateTime, Resolution, Symbol, TickType};
use lean_data::{CustomDataPoint, TradeBar};
use rust_decimal::prelude::ToPrimitive;
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub symbols: Vec<Symbol>,
    pub start: DateTime,
    pub end: DateTime,
    pub resolution: Resolution,
    pub tick_type: TickType,
}

impl HistoryRequest {
    pub fn new(
        symbols: Vec<Symbol>,
        start: DateTime,
        end: DateTime,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Self {
        Self {
            symbols,
            start,
            end,
            resolution,
            tick_type,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HistoryResult {
    pub trade_bars: Vec<TradeBar>,
}

impl HistoryResult {
    pub fn from_trade_bars(trade_bars: Vec<TradeBar>) -> Self {
        Self { trade_bars }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum AlgorithmHistoryRange {
    Count {
        start: NaiveDate,
        end: NaiveDate,
        bar_count: usize,
    },
    Dates {
        start: NaiveDate,
        end: NaiveDate,
    },
}

impl AlgorithmHistoryRange {
    pub fn for_count(bar_count: usize, end: NaiveDate) -> Self {
        let calendar_days = (bar_count as i64 * 7 + 4) / 5 + 10;
        Self::Count {
            start: end - chrono::Duration::days(calendar_days),
            end,
            bar_count,
        }
    }

    pub fn for_dates(start: NaiveDate, end: NaiveDate) -> Self {
        Self::Dates { start, end }
    }

    pub fn start(self) -> NaiveDate {
        match self {
            Self::Count { start, .. } | Self::Dates { start, .. } => start,
        }
    }

    pub fn end(self) -> NaiveDate {
        match self {
            Self::Count { end, .. } | Self::Dates { end, .. } => end,
        }
    }

    pub fn bar_count(self) -> Option<usize> {
        match self {
            Self::Count { bar_count, .. } => Some(bar_count),
            Self::Dates { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TradeBarHistoryColumns {
    pub time: Vec<String>,
    pub open: Vec<f64>,
    pub high: Vec<f64>,
    pub low: Vec<f64>,
    pub close: Vec<f64>,
    pub volume: Vec<f64>,
}

impl TradeBarHistoryColumns {
    pub fn from_bars(mut bars: Vec<TradeBar>, bar_count: Option<usize>) -> Self {
        bars.sort_by_key(|b| b.time.0);
        if let Some(bar_count) = bar_count {
            if bars.len() > bar_count {
                bars = bars[bars.len() - bar_count..].to_vec();
            }
        }
        Self {
            time: bars.iter().map(|b| date_string(b.time)).collect(),
            open: bars
                .iter()
                .map(|b| b.open.to_f64().unwrap_or(0.0))
                .collect(),
            high: bars
                .iter()
                .map(|b| b.high.to_f64().unwrap_or(0.0))
                .collect(),
            low: bars.iter().map(|b| b.low.to_f64().unwrap_or(0.0)).collect(),
            close: bars
                .iter()
                .map(|b| b.close.to_f64().unwrap_or(0.0))
                .collect(),
            volume: bars
                .iter()
                .map(|b| b.volume.to_f64().unwrap_or(0.0))
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct CustomDataHistoryColumns {
    pub time: Vec<String>,
    pub end_time: Vec<String>,
    pub value: Vec<f64>,
    pub fields: Vec<(String, Vec<Option<serde_json::Value>>)>,
}

impl CustomDataHistoryColumns {
    pub fn from_points(mut points: Vec<CustomDataPoint>, bar_count: Option<usize>) -> Self {
        points.sort_by_key(custom_point_sort_key);
        if let Some(bar_count) = bar_count {
            points = filter_custom_points_by_last_dates(points, bar_count);
        }

        let mut field_names: Vec<String> = points
            .iter()
            .flat_map(|p| p.fields.keys().cloned())
            .collect();
        field_names.sort();
        field_names.dedup();

        let fields = field_names
            .into_iter()
            .map(|field| {
                let values = points
                    .iter()
                    .map(|point| point.fields.get(&field).cloned())
                    .collect();
                (field, values)
            })
            .collect();

        Self {
            time: points.iter().map(|p| p.time.to_string()).collect(),
            end_time: points
                .iter()
                .map(|p| {
                    p.end_time
                        .map(iso_string)
                        .unwrap_or_else(|| p.time.to_string())
                })
                .collect(),
            value: points
                .iter()
                .map(|p| p.value.to_f64().unwrap_or(0.0))
                .collect(),
            fields,
        }
    }
}

pub fn iso_string(dt: DateTime) -> String {
    let secs = dt.0 / 1_000_000_000;
    let nsub = (dt.0 % 1_000_000_000) as u32;
    let dt: chrono::DateTime<Utc> =
        chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    dt.format("%Y-%m-%dT%H:%M:%S").to_string()
}

pub fn date_string(dt: DateTime) -> String {
    let secs = dt.0 / 1_000_000_000;
    let nsub = (dt.0 % 1_000_000_000) as u32;
    let dt: chrono::DateTime<Utc> =
        chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    dt.format("%Y-%m-%d").to_string()
}

pub fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap_or_default()))
}

fn custom_point_sort_key(point: &CustomDataPoint) -> i64 {
    point
        .end_time
        .map(|t| t.0)
        .unwrap_or_else(|| date_to_datetime(point.time, 0, 0, 0).0)
}

pub fn filter_custom_points_by_last_dates(
    points: Vec<CustomDataPoint>,
    bar_count: usize,
) -> Vec<CustomDataPoint> {
    if bar_count == 0 || points.is_empty() {
        return Vec::new();
    }

    let mut dates: Vec<NaiveDate> = points
        .iter()
        .map(|p| p.end_time.map(|t| t.date_utc()).unwrap_or(p.time))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if dates.len() <= bar_count {
        return points;
    }

    dates = dates.split_off(dates.len() - bar_count);
    let keep: BTreeSet<NaiveDate> = dates.into_iter().collect();
    points
        .into_iter()
        .filter(|p| keep.contains(&p.end_time.map(|t| t.date_utc()).unwrap_or(p.time)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use lean_core::{Market, Symbol, TimeSpan};
    use lean_data::TradeBarData;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn bar(day: u32, close: i64) -> TradeBar {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let time = date_to_datetime(date(2024, 1, day), 0, 0, 0);
        TradeBar::new(
            symbol,
            time,
            TimeSpan::from_days(1),
            TradeBarData::new(dec!(1), dec!(2), dec!(0.5), Decimal::from(close), dec!(100)),
        )
    }

    fn point(day: u32, value: i64, fields: HashMap<String, serde_json::Value>) -> CustomDataPoint {
        CustomDataPoint {
            time: date(2024, 1, day),
            end_time: Some(date_to_datetime(date(2024, 1, day), 16, 0, 0)),
            value: Decimal::from(value),
            symbol: None,
            fields: Arc::new(fields),
        }
    }

    #[test]
    fn history_count_range_uses_calendar_buffer_and_records_bar_count() {
        let end = date(2024, 1, 31);
        let range = AlgorithmHistoryRange::for_count(5, end);

        assert_eq!(range.end(), end);
        assert_eq!(range.bar_count(), Some(5));
        assert_eq!(range.start(), date(2024, 1, 14));
    }

    #[test]
    fn trade_bar_columns_sort_and_keep_last_count() {
        let columns =
            TradeBarHistoryColumns::from_bars(vec![bar(3, 30), bar(1, 10), bar(2, 20)], Some(2));

        assert_eq!(columns.time, vec!["2024-01-02", "2024-01-03"]);
        assert_eq!(columns.close, vec![20.0, 30.0]);
        assert_eq!(columns.open, vec![1.0, 1.0]);
        assert_eq!(columns.volume, vec![100.0, 100.0]);
    }

    #[test]
    fn custom_data_columns_align_sorted_sparse_fields() {
        let columns = CustomDataHistoryColumns::from_points(
            vec![
                point(2, 20, HashMap::from([("beta".to_string(), json!(2))])),
                point(1, 10, HashMap::from([("alpha".to_string(), json!("a"))])),
            ],
            None,
        );

        assert_eq!(columns.time, vec!["2024-01-01", "2024-01-02"]);
        assert_eq!(columns.value, vec![10.0, 20.0]);
        assert_eq!(columns.fields[0].0, "alpha");
        assert_eq!(columns.fields[0].1, vec![Some(json!("a")), None]);
        assert_eq!(columns.fields[1].0, "beta");
        assert_eq!(columns.fields[1].1, vec![None, Some(json!(2))]);
    }

    #[test]
    fn custom_data_filter_keeps_all_points_for_last_dates() {
        let points = vec![
            point(1, 10, HashMap::new()),
            point(2, 20, HashMap::new()),
            point(2, 21, HashMap::new()),
            point(3, 30, HashMap::new()),
        ];

        let kept = filter_custom_points_by_last_dates(points, 2);

        assert_eq!(kept.len(), 3);
        assert_eq!(kept[0].time, date(2024, 1, 2));
        assert_eq!(kept[1].time, date(2024, 1, 2));
        assert_eq!(kept[2].time, date(2024, 1, 3));
        assert!(filter_custom_points_by_last_dates(kept, 0).is_empty());
    }
}
