use crate::py_data::PyTradeBar;
use crate::py_types::PyIndicatorResult;
use lean_core::NanosecondTimestamp;
use lean_indicators::indicator::Indicator;
use lean_indicators::{Atr, BollingerBands, Ema, Macd, Rsi, Sma};
use pyo3::prelude::*;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

// Placeholder timestamp — indicators don't care about the exact time for computation.
fn dummy_ts() -> NanosecondTimestamp {
    NanosecondTimestamp::EPOCH
}

fn f2d(f: f64) -> Decimal {
    Decimal::from_f64(f).unwrap_or_default()
}

fn make_result(r: lean_indicators::indicator::IndicatorResult) -> PyIndicatorResult {
    PyIndicatorResult {
        is_ready: r.is_ready(),
        value: r.value.to_f64().unwrap_or(0.0),
    }
}

fn bar_from_py(bar: &PyTradeBar) -> lean_data::TradeBar {
    use lean_core::{Market, TimeSpan};
    lean_data::TradeBar {
        symbol: lean_core::Symbol::create_equity(&bar.symbol.inner.value, &Market::usa()),
        time: dummy_ts(),
        end_time: dummy_ts(),
        open: f2d(bar.open),
        high: f2d(bar.high),
        low: f2d(bar.low),
        close: f2d(bar.close),
        volume: f2d(bar.volume),
        period: TimeSpan::ONE_DAY,
    }
}

// ─── IndicatorDataPoint ───────────────────────────────────────────────────────
// LEAN indicators expose `.current` which returns an IndicatorDataPoint with
// `.value` and `.time` fields.

#[pyclass(name = "IndicatorDataPoint", frozen)]
#[derive(Debug, Clone)]
pub struct PyIndicatorDataPoint {
    pub value: f64,
}

#[pymethods]
impl PyIndicatorDataPoint {
    #[getter]
    fn value(&self) -> f64 {
        self.value
    }

    #[getter]
    fn time(&self) -> PyObject {
        // Return a Python datetime representing epoch (placeholder).
        Python::with_gil(|py| {
            py.import("datetime")
                .and_then(|m| m.getattr("datetime"))
                .and_then(|dt| dt.call_method1("utcfromtimestamp", (0.0f64,)))
                .map(|o| o.into())
                .unwrap_or_else(|_| py.None())
        })
    }

    fn __repr__(&self) -> String {
        format!("IndicatorDataPoint(value={:.6})", self.value)
    }
}

// ─── SMA ─────────────────────────────────────────────────────────────────────

#[pyclass(name = "SimpleMovingAverage")]
pub struct PySma {
    inner: Sma,
}

#[pymethods]
impl PySma {
    #[new]
    fn new(period: usize) -> Self {
        PySma {
            inner: Sma::new(period),
        }
    }

    /// LEAN API: update(time, value) — time arg is accepted but ignored for computation.
    #[pyo3(signature = (time_or_value, value=None))]
    fn update(
        &mut self,
        time_or_value: &Bound<'_, PyAny>,
        value: Option<f64>,
    ) -> PyResult<PyIndicatorResult> {
        let price = if let Some(v) = value {
            v
        } else {
            time_or_value.extract::<f64>()?
        };
        Ok(make_result(self.inner.update_price(dummy_ts(), f2d(price))))
    }

    fn update_bar(&mut self, bar: &PyTradeBar) -> PyIndicatorResult {
        make_result(self.inner.update_bar(&bar_from_py(bar)))
    }

    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    #[getter]
    fn value(&self) -> f64 {
        self.inner.current().value.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn samples(&self) -> usize {
        self.inner.samples()
    }
    #[getter]
    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    /// LEAN API: `.current` returns an IndicatorDataPoint with `.value`.
    #[getter]
    fn current(&self) -> PyIndicatorDataPoint {
        PyIndicatorDataPoint {
            value: self.inner.current().value.to_f64().unwrap_or(0.0),
        }
    }

    fn reset(&mut self) {
        self.inner.reset()
    }
    fn __repr__(&self) -> String {
        format!(
            "SimpleMovingAverage(period={}, ready={})",
            self.inner.warm_up_period(),
            self.inner.is_ready()
        )
    }
}

// ─── EMA ─────────────────────────────────────────────────────────────────────

#[pyclass(name = "ExponentialMovingAverage")]
pub struct PyEma {
    inner: Ema,
}

#[pymethods]
impl PyEma {
    #[new]
    fn new(period: usize) -> Self {
        PyEma {
            inner: Ema::new(period),
        }
    }

    /// LEAN API: update(time, value) — time arg is accepted but ignored for computation.
    #[pyo3(signature = (time_or_value, value=None))]
    fn update(
        &mut self,
        time_or_value: &Bound<'_, PyAny>,
        value: Option<f64>,
    ) -> PyResult<PyIndicatorResult> {
        let price = if let Some(v) = value {
            v
        } else {
            time_or_value.extract::<f64>()?
        };
        Ok(make_result(self.inner.update_price(dummy_ts(), f2d(price))))
    }

    fn update_bar(&mut self, bar: &PyTradeBar) -> PyIndicatorResult {
        make_result(self.inner.update_bar(&bar_from_py(bar)))
    }

    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    #[getter]
    fn value(&self) -> f64 {
        self.inner.current().value.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn samples(&self) -> usize {
        self.inner.samples()
    }
    #[getter]
    fn warm_up_period(&self) -> usize {
        self.inner.warm_up_period()
    }

    /// LEAN API: `.current` returns an IndicatorDataPoint with `.value`.
    #[getter]
    fn current(&self) -> PyIndicatorDataPoint {
        PyIndicatorDataPoint {
            value: self.inner.current().value.to_f64().unwrap_or(0.0),
        }
    }

    fn reset(&mut self) {
        self.inner.reset()
    }
    fn __repr__(&self) -> String {
        format!(
            "ExponentialMovingAverage(period={}, ready={})",
            self.inner.warm_up_period(),
            self.inner.is_ready()
        )
    }
}

// ─── RSI ─────────────────────────────────────────────────────────────────────

#[pyclass(name = "RelativeStrengthIndex")]
pub struct PyRsi {
    inner: Rsi,
}

#[pymethods]
impl PyRsi {
    #[new]
    fn new(period: usize) -> Self {
        PyRsi {
            inner: Rsi::new(period),
        }
    }

    /// LEAN API: update(time, value) — time arg is accepted but ignored for computation.
    #[pyo3(signature = (time_or_value, value=None))]
    fn update(
        &mut self,
        time_or_value: &Bound<'_, PyAny>,
        value: Option<f64>,
    ) -> PyResult<PyIndicatorResult> {
        let price = if let Some(v) = value {
            v
        } else {
            time_or_value.extract::<f64>()?
        };
        Ok(make_result(self.inner.update_price(dummy_ts(), f2d(price))))
    }

    fn update_bar(&mut self, bar: &PyTradeBar) -> PyIndicatorResult {
        make_result(self.inner.update_bar(&bar_from_py(bar)))
    }

    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    #[getter]
    fn value(&self) -> f64 {
        self.inner.current().value.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn is_overbought(&self) -> bool {
        self.inner.is_overbought()
    }
    #[getter]
    fn is_oversold(&self) -> bool {
        self.inner.is_oversold()
    }
    #[getter]
    fn samples(&self) -> usize {
        self.inner.samples()
    }

    /// LEAN API: `.current` returns an IndicatorDataPoint with `.value`.
    #[getter]
    fn current(&self) -> PyIndicatorDataPoint {
        PyIndicatorDataPoint {
            value: self.inner.current().value.to_f64().unwrap_or(0.0),
        }
    }

    fn reset(&mut self) {
        self.inner.reset()
    }
    fn __repr__(&self) -> String {
        format!(
            "RelativeStrengthIndex(value={:.2}, ready={})",
            self.inner.current().value,
            self.inner.is_ready()
        )
    }
}

// ─── MACD ────────────────────────────────────────────────────────────────────

#[pyclass(name = "MovingAverageConvergenceDivergence")]
pub struct PyMacd {
    inner: Macd,
}

#[pymethods]
impl PyMacd {
    #[new]
    fn new(fast: usize, slow: usize, signal: usize) -> Self {
        PyMacd {
            inner: Macd::new(fast, slow, signal),
        }
    }

    /// LEAN API: update(time, value) — time arg is accepted but ignored for computation.
    #[pyo3(signature = (time_or_value, value=None))]
    fn update(
        &mut self,
        time_or_value: &Bound<'_, PyAny>,
        value: Option<f64>,
    ) -> PyResult<PyIndicatorResult> {
        let price = if let Some(v) = value {
            v
        } else {
            time_or_value.extract::<f64>()?
        };
        Ok(make_result(self.inner.update_price(dummy_ts(), f2d(price))))
    }

    fn update_bar(&mut self, bar: &PyTradeBar) -> PyIndicatorResult {
        make_result(self.inner.update_bar(&bar_from_py(bar)))
    }

    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    #[getter]
    fn value(&self) -> f64 {
        self.inner.current().value.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn macd_line(&self) -> f64 {
        self.inner.macd_line.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn signal_line(&self) -> f64 {
        self.inner.signal_line.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn histogram(&self) -> f64 {
        self.inner.histogram.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn samples(&self) -> usize {
        self.inner.samples()
    }

    /// LEAN API: `.current` returns an IndicatorDataPoint with `.value`.
    #[getter]
    fn current(&self) -> PyIndicatorDataPoint {
        PyIndicatorDataPoint {
            value: self.inner.current().value.to_f64().unwrap_or(0.0),
        }
    }

    fn reset(&mut self) {
        self.inner.reset()
    }
}

// ─── Bollinger Bands ─────────────────────────────────────────────────────────

#[pyclass(name = "BollingerBands")]
pub struct PyBollingerBands {
    inner: BollingerBands,
}

#[pymethods]
impl PyBollingerBands {
    #[new]
    #[pyo3(signature = (period, k=2.0))]
    fn new(period: usize, k: f64) -> Self {
        PyBollingerBands {
            inner: BollingerBands::new(period, f2d(k)),
        }
    }

    /// LEAN API: update(time, value) — time arg is accepted but ignored for computation.
    #[pyo3(signature = (time_or_value, value=None))]
    fn update(
        &mut self,
        time_or_value: &Bound<'_, PyAny>,
        value: Option<f64>,
    ) -> PyResult<PyIndicatorResult> {
        let price = if let Some(v) = value {
            v
        } else {
            time_or_value.extract::<f64>()?
        };
        Ok(make_result(self.inner.update_price(dummy_ts(), f2d(price))))
    }

    fn update_bar(&mut self, bar: &PyTradeBar) -> PyIndicatorResult {
        make_result(self.inner.update_bar(&bar_from_py(bar)))
    }

    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    #[getter]
    fn middle(&self) -> f64 {
        self.inner.middle.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn upper(&self) -> f64 {
        self.inner.upper.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn lower(&self) -> f64 {
        self.inner.lower.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn bandwidth(&self) -> f64 {
        self.inner.bandwidth.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn percent_b(&self) -> f64 {
        self.inner.percent_b.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn samples(&self) -> usize {
        self.inner.samples()
    }

    /// LEAN API: `.current` returns an IndicatorDataPoint with `.value`.
    #[getter]
    fn current(&self) -> PyIndicatorDataPoint {
        PyIndicatorDataPoint {
            value: self.inner.middle.to_f64().unwrap_or(0.0),
        }
    }

    fn reset(&mut self) {
        self.inner.reset()
    }
}

// ─── ATR ─────────────────────────────────────────────────────────────────────

#[pyclass(name = "AverageTrueRange")]
pub struct PyAtr {
    inner: Atr,
}

#[pymethods]
impl PyAtr {
    #[new]
    fn new(period: usize) -> Self {
        PyAtr {
            inner: Atr::new(period),
        }
    }

    /// ATR requires OHLC data — only update_bar is meaningful.
    fn update_bar(&mut self, bar: &PyTradeBar) -> PyIndicatorResult {
        make_result(self.inner.update_bar(&bar_from_py(bar)))
    }

    #[getter]
    fn is_ready(&self) -> bool {
        self.inner.is_ready()
    }
    #[getter]
    fn value(&self) -> f64 {
        self.inner.current().value.to_f64().unwrap_or(0.0)
    }
    #[getter]
    fn samples(&self) -> usize {
        self.inner.samples()
    }

    /// LEAN API: `.current` returns an IndicatorDataPoint with `.value`.
    #[getter]
    fn current(&self) -> PyIndicatorDataPoint {
        PyIndicatorDataPoint {
            value: self.inner.current().value.to_f64().unwrap_or(0.0),
        }
    }

    fn reset(&mut self) {
        self.inner.reset()
    }
}
