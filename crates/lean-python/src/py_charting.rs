use pyo3::prelude::*;
use crate::charting::{ChartCollection, SeriesType};
use std::sync::{Arc, Mutex};

/// Thread-safe chart collection for use from Python
#[pyclass(name = "ChartCollection")]
pub struct PyChartCollection {
    pub inner: Arc<Mutex<ChartCollection>>,
}

#[pymethods]
impl PyChartCollection {
    #[new]
    fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(ChartCollection::new())) }
    }

    /// Plot a value on a line chart.
    /// chart: chart name (e.g. "Strategy")
    /// series: series name (e.g. "RSI")
    /// time: date string "YYYY-MM-DD"
    /// value: the value to plot
    fn plot(&self, chart: &str, series: &str, time: &str, value: f64) {
        if let Ok(mut c) = self.inner.lock() {
            c.plot(chart, series, time, value);
        }
    }
}

impl PyChartCollection {
    /// Create a PyChartCollection wrapping an existing Arc.
    pub fn from_arc(inner: Arc<Mutex<ChartCollection>>) -> Self {
        Self { inner }
    }
}
