pub mod charting;
pub mod py_charting;
pub mod py_types;
pub mod py_data;
pub mod py_orders;
pub mod py_indicators;
pub mod py_portfolio;
pub mod py_options;
pub mod py_qc_algorithm;
pub mod py_quant_book;
pub mod py_adapter;
pub mod runner;
pub mod report;

use pyo3::prelude::*;

use py_charting::PyChartCollection;
use py_types::{PyResolution, PySecurity, PySymbol, PyIndicatorResult};
use py_data::{PySlice, PyTradeBar, PyTradeBars};
use py_orders::PyOrderEvent;
use py_indicators::{PySma, PyEma, PyRsi, PyMacd, PyBollingerBands, PyAtr, PyIndicatorDataPoint};
use py_portfolio::{PyPortfolio, PySecurityHolding};
use py_qc_algorithm::PyQcAlgorithm;
use py_quant_book::PyQuantBook;

// ─── Additional enums matching LEAN's Python API ──────────────────────────────

/// LEAN SecurityType enum values.
#[pyclass(name = "SecurityType", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PySecurityType {
    Base    = 0,
    Equity  = 1,
    Option  = 2,
    Forex   = 3,
    Future  = 4,
    Cfd     = 5,
    Crypto  = 7,
    Index   = 8,
}

/// LEAN OrderType enum values.
#[pyclass(name = "OrderType", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyOrderType {
    Market      = 0,
    Limit       = 1,
    StopMarket  = 2,
    StopLimit   = 3,
    MarketOnClose = 4,
    OptionExercise = 5,
    LimitIfTouched = 6,
    ComboMarket = 7,
    ComboLimit  = 8,
    ComboLegLimit = 9,
    TrailingStop = 10,
}

/// LEAN OrderStatus enum values.
#[pyclass(name = "OrderStatus", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyOrderStatus {
    New             = 0,
    Submitted       = 1,
    PartiallyFilled = 2,
    Filled          = 3,
    Canceled        = 5,
    Invalid         = 6,
    CancelPending   = 7,
    UpdateSubmitted = 8,
}

/// The `AlgorithmImports` Python module — importable as `from AlgorithmImports import *`.
///
/// Register before starting the interpreter:
/// ```rust
/// pyo3::append_to_inittab!(AlgorithmImports);
/// pyo3::prepare_freethreaded_python();
/// ```
///
/// Then in Python strategies:
/// ```python
/// from AlgorithmImports import *
///
/// class MyAlgo(QCAlgorithm):
///     def initialize(self):
///         self.spy = self.add_equity("SPY", Resolution.Daily).symbol
///         self.fast = SimpleMovingAverage(50)
/// ```
#[pymodule]
#[allow(non_snake_case)]
pub fn AlgorithmImports(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Core types
    m.add_class::<PyResolution>()?;
    m.add_class::<PySymbol>()?;
    m.add_class::<PySecurity>()?;
    m.add_class::<PyIndicatorResult>()?;
    m.add_class::<PyIndicatorDataPoint>()?;

    // Data
    m.add_class::<PyTradeBar>()?;
    m.add_class::<PyTradeBars>()?;
    m.add_class::<PySlice>()?;

    // Orders
    m.add_class::<PyOrderEvent>()?;

    // Portfolio
    m.add_class::<PySecurityHolding>()?;
    m.add_class::<PyPortfolio>()?;

    // Options
    m.add_class::<py_options::PyOptionRight>()?;
    m.add_class::<py_options::PyGreeks>()?;
    m.add_class::<py_options::PyOptionContract>()?;
    m.add_class::<py_options::PyOptionChain>()?;

    // Charting
    m.add_class::<PyChartCollection>()?;

    // Algorithm base class (LEAN name: QCAlgorithm)
    m.add_class::<PyQcAlgorithm>()?;

    // Research / notebook
    m.add_class::<PyQuantBook>()?;

    // Indicators (LEAN names)
    m.add_class::<PySma>()?;
    m.add_class::<PyEma>()?;
    m.add_class::<PyRsi>()?;
    m.add_class::<PyMacd>()?;
    m.add_class::<PyBollingerBands>()?;
    m.add_class::<PyAtr>()?;

    // Additional enums
    m.add_class::<PySecurityType>()?;
    m.add_class::<PyOrderType>()?;
    m.add_class::<PyOrderStatus>()?;

    Ok(())
}

/// Backward compatibility alias — `lean_rust` was the old module name.
/// Keep this so existing internal code that calls `pyo3::append_to_inittab!(lean_rust)`
/// still compiles, though new code should use `AlgorithmImports`.
#[allow(non_snake_case)]
pub use AlgorithmImports as lean_rust;

#[cfg(test)]
mod tests {
    use super::*;
    use pyo3::prelude::*;

    /// Integration test: verify that the AlgorithmImports module exposes the
    /// correct LEAN-compatible API surface from Python.
    #[test]
    fn test_algorithm_imports_api() {
        pyo3::append_to_inittab!(AlgorithmImports);
        pyo3::prepare_freethreaded_python();

        Python::with_gil(|py| {
            // Test 1: Resolution.Daily is accessible (PascalCase, not SCREAMING_SNAKE_CASE)
            let result = py.run(
                c"
from AlgorithmImports import Resolution
assert Resolution.Daily is not None, 'Resolution.Daily must be accessible'
assert not hasattr(Resolution, 'DAILY'), 'Resolution.DAILY must NOT exist (use Daily)'
",
                None,
                None,
            );
            assert!(result.is_ok(), "Resolution.Daily test failed: {:?}", result);

            // Test 2: SimpleMovingAverage(50) creates an indicator
            let result = py.run(
                c"
from AlgorithmImports import SimpleMovingAverage
sma = SimpleMovingAverage(50)
assert sma is not None, 'SimpleMovingAverage(50) must not be None'
assert not sma.is_ready, 'new SMA should not be ready'
",
                None,
                None,
            );
            assert!(result.is_ok(), "SimpleMovingAverage creation test failed: {:?}", result);

            // Test 3: .update(datetime, value) works with time arg
            let result = py.run(
                c"
import datetime
from AlgorithmImports import SimpleMovingAverage
sma = SimpleMovingAverage(3)
# Feed 3 values with datetime arg (LEAN API: update(time, value))
for i in range(1, 4):
    sma.update(datetime.datetime.now(), float(i * 10))
assert sma.is_ready, 'SMA with period=3 should be ready after 3 updates'
",
                None,
                None,
            );
            assert!(result.is_ok(), "SMA update(time, value) test failed: {:?}", result);

            // Test 4: .current.value returns a float
            let result = py.run(
                c"
import datetime
from AlgorithmImports import SimpleMovingAverage
sma = SimpleMovingAverage(3)
for i in range(1, 4):
    sma.update(datetime.datetime.now(), float(i * 10))
val = sma.current.value
assert isinstance(val, float), 'current.value must be float, got {}'.format(type(val))
assert val > 0, 'current.value must be positive, got {}'.format(val)
",
                None,
                None,
            );
            assert!(result.is_ok(), ".current.value test failed: {:?}", result);

            // Test 5: QCAlgorithm is exposed (not QcAlgorithm)
            let result = py.run(
                c"
from AlgorithmImports import QCAlgorithm
assert QCAlgorithm is not None, 'QCAlgorithm must be accessible'
",
                None,
                None,
            );
            assert!(result.is_ok(), "QCAlgorithm name test failed: {:?}", result);

            // Test 6: All expected LEAN indicator names are present
            let result = py.run(
                c"
from AlgorithmImports import (
    SimpleMovingAverage,
    ExponentialMovingAverage,
    RelativeStrengthIndex,
    MovingAverageConvergenceDivergence,
    BollingerBands,
    AverageTrueRange,
)
assert SimpleMovingAverage is not None
assert ExponentialMovingAverage is not None
assert RelativeStrengthIndex is not None
assert MovingAverageConvergenceDivergence is not None
assert BollingerBands is not None
assert AverageTrueRange is not None
",
                None,
                None,
            );
            assert!(result.is_ok(), "Indicator names test failed: {:?}", result);
        });
    }
}
