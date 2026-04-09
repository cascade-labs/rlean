use std::collections::HashMap;
use serde::{Deserialize, Serialize};

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
    pub time: String,   // ISO date string "YYYY-MM-DD"
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Series {
    pub name: String,
    pub series_type: SeriesType,
    pub color: Option<String>,      // hex color e.g. "#2196F3"
    pub unit: String,               // e.g. "$", "%", ""
    pub points: Vec<ChartPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chart {
    pub name: String,
    pub series: HashMap<String, Series>,
}

impl Chart {
    pub fn new(name: &str) -> Self {
        Self { name: name.to_string(), series: HashMap::new() }
    }
    pub fn add_series(&mut self, series: Series) {
        self.series.insert(series.name.clone(), series);
    }
    pub fn get_or_create_series(&mut self, name: &str, series_type: SeriesType) -> &mut Series {
        self.series.entry(name.to_string()).or_insert_with(|| Series {
            name: name.to_string(),
            series_type,
            color: None,
            unit: String::new(),
            points: Vec::new(),
        })
    }
}

/// Holds all charts for a backtest. Shared across the runner and strategy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChartCollection {
    pub charts: HashMap<String, Chart>,
}

impl ChartCollection {
    pub fn new() -> Self { Self::default() }
    pub fn get_or_create(&mut self, chart_name: &str) -> &mut Chart {
        self.charts.entry(chart_name.to_string()).or_insert_with(|| Chart::new(chart_name))
    }
    pub fn plot(&mut self, chart: &str, series: &str, time: &str, value: f64) {
        let chart = self.get_or_create(chart);
        let s = chart.get_or_create_series(series, SeriesType::Line);
        s.points.push(ChartPoint { time: time.to_string(), value });
    }
}
