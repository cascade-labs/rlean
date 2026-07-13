//! Algorithm-owned charting state and primitives.
//!
//! C# LEAN treats charts as `QCAlgorithm`/result state (`Plot`, `AddChart`,
//! chart updates) consumed by the engine result/reporting layer. SDK bindings
//! should wrap these types rather than own the charting behavior.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SeriesType {
    Line,
    Scatter,
    Bar,
    Candle,
    Flag,
    StackedArea,
    Pie,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChartPoint {
    pub time: String,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Series {
    pub name: String,
    pub series_type: SeriesType,
    pub color: Option<String>,
    pub unit: String,
    pub points: Vec<ChartPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chart {
    pub name: String,
    pub series: HashMap<String, Series>,
}

impl Chart {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            series: HashMap::new(),
        }
    }

    pub fn add_series(&mut self, series: Series) {
        self.series.insert(series.name.clone(), series);
    }

    pub fn get_or_create_series(&mut self, name: &str, series_type: SeriesType) -> &mut Series {
        self.series
            .entry(name.to_string())
            .or_insert_with(|| Series {
                name: name.to_string(),
                series_type,
                color: None,
                unit: String::new(),
                points: Vec::new(),
            })
    }
}

/// Holds all charts for an algorithm run. Shared across the runner and strategy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChartCollection {
    pub charts: HashMap<String, Chart>,
}

impl ChartCollection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_or_create(&mut self, chart_name: &str) -> &mut Chart {
        self.charts
            .entry(chart_name.to_string())
            .or_insert_with(|| Chart::new(chart_name))
    }

    pub fn plot(&mut self, chart: &str, series: &str, time: &str, value: f64) {
        let chart = self.get_or_create(chart);
        let series = chart.get_or_create_series(series, SeriesType::Line);
        series.points.push(ChartPoint {
            time: time.to_string(),
            value,
        });
    }
}

pub type SharedChartCollection = Arc<Mutex<ChartCollection>>;

pub fn new_shared_chart_collection() -> SharedChartCollection {
    Arc::new(Mutex::new(ChartCollection::new()))
}

pub fn plot_shared_chart(
    charts: &SharedChartCollection,
    chart: &str,
    series: &str,
    time: &str,
    value: f64,
) {
    if let Ok(mut charts) = charts.lock() {
        charts.plot(chart, series, time, value);
    }
}

pub fn ensure_shared_chart(charts: &SharedChartCollection, name: &str) {
    if let Ok(mut charts) = charts.lock() {
        charts.get_or_create(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plot_creates_chart_series_and_point() {
        let charts = new_shared_chart_collection();
        plot_shared_chart(&charts, "Strategy", "RSI", "2020-01-01", 42.5);

        let charts = charts.lock().unwrap();
        let chart = charts.charts.get("Strategy").expect("chart created");
        let series = chart.series.get("RSI").expect("series created");
        assert_eq!(series.series_type, SeriesType::Line);
        assert_eq!(series.points.len(), 1);
        assert_eq!(series.points[0].time, "2020-01-01");
        assert!((series.points[0].value - 42.5).abs() < 1e-9);
    }

    #[test]
    fn plot_appends_points_to_existing_series() {
        let mut charts = ChartCollection::new();
        charts.plot("Strategy", "RSI", "2020-01-01", 1.0);
        charts.plot("Strategy", "RSI", "2020-01-02", 2.0);

        let series = charts
            .charts
            .get("Strategy")
            .and_then(|chart| chart.series.get("RSI"))
            .expect("series created");
        assert_eq!(series.points.len(), 2);
        assert_eq!(charts.charts.len(), 1);
        assert_eq!(charts.charts["Strategy"].series.len(), 1);
    }
}
