//! SDK charting binding handles.
//!
//! Chart state and behavior live in `lean-algorithm`; this module only exposes
//! SDK/Python handles over the Rust-owned chart collection.

use lean_algorithm::charting::{
    new_shared_chart_collection, plot_shared_chart, SharedChartCollection,
};

/// SDK-owned handle wrapping a [`SharedChartCollection`] for language bindings.
///
/// All charting behavior (locking, plotting) lives here so the generated Python
/// layer is pure marshalling. Cloning shares the underlying collection.
#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "ChartCollection"))]
pub struct ChartCollectionHandle {
    inner: SharedChartCollection,
}

impl ChartCollectionHandle {
    /// LEAN-compatible constructor exposed to Python as `ChartCollection()`.
    pub fn new() -> Self {
        Self {
            inner: new_shared_chart_collection(),
        }
    }

    /// Wrap an existing shared collection (e.g. the algorithm's charts).
    pub fn from_shared(inner: SharedChartCollection) -> Self {
        Self { inner }
    }

    /// Borrow the underlying shared collection.
    pub fn shared(&self) -> &SharedChartCollection {
        &self.inner
    }

    /// Plot a value on a line chart, creating the chart/series on demand.
    pub fn plot(&self, chart: &str, series: &str, time: &str, value: f64) {
        plot_shared_chart(&self.inner, chart, series, time, value);
    }
}

impl Default for ChartCollectionHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl ChartCollectionHandle {
    #[new]
    fn py_new() -> Self {
        Self::new()
    }

    #[pyo3(name = "plot")]
    fn py_plot(&self, chart: &str, series: &str, time: &str, value: f64) {
        self.plot(chart, series, time, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_algorithm::charting::SeriesType;

    #[test]
    fn plot_creates_chart_series_and_point() {
        let handle = ChartCollectionHandle::new();
        handle.plot("Strategy", "RSI", "2020-01-01", 42.5);

        let charts = handle.shared().lock().unwrap();
        let chart = charts.charts.get("Strategy").expect("chart created");
        let series = chart.series.get("RSI").expect("series created");
        assert_eq!(series.series_type, SeriesType::Line);
        assert_eq!(series.points.len(), 1);
        assert_eq!(series.points[0].time, "2020-01-01");
        assert!((series.points[0].value - 42.5).abs() < 1e-9);
    }

    #[test]
    fn plot_appends_points_to_existing_series() {
        let handle = ChartCollectionHandle::new();
        handle.plot("Strategy", "RSI", "2020-01-01", 1.0);
        handle.plot("Strategy", "RSI", "2020-01-02", 2.0);

        let charts = handle.shared().lock().unwrap();
        let series = charts
            .charts
            .get("Strategy")
            .and_then(|chart| chart.series.get("RSI"))
            .expect("series created");
        assert_eq!(series.points.len(), 2);
        assert_eq!(charts.charts.len(), 1);
        assert_eq!(charts.charts["Strategy"].series.len(), 1);
    }

    #[test]
    fn from_shared_shares_underlying_collection() {
        let shared = new_shared_chart_collection();
        let handle = ChartCollectionHandle::from_shared(shared.clone());
        handle.plot("Chart", "Series", "2020-01-01", 7.0);

        let charts = shared.lock().unwrap();
        let series = charts
            .charts
            .get("Chart")
            .and_then(|chart| chart.series.get("Series"))
            .expect("series visible through original Arc");
        assert_eq!(series.points.len(), 1);
        assert!((series.points[0].value - 7.0).abs() < 1e-9);
    }

    #[test]
    fn clone_shares_underlying_collection() {
        let handle = ChartCollectionHandle::new();
        let clone = handle.clone();
        clone.plot("Chart", "Series", "2020-01-01", 3.0);

        let charts = handle.shared().lock().unwrap();
        assert_eq!(charts.charts["Chart"].series["Series"].points.len(), 1);
    }
}
