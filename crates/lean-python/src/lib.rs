pub mod charting;
pub mod py_adapter;
pub mod py_charting;
pub mod py_data;
pub mod py_framework;
pub mod py_indicators;
pub mod py_options;
pub mod py_orders;
pub mod py_portfolio;
pub mod py_qc_algorithm;
pub mod py_quant_book;
pub mod py_types;
pub mod report;
pub mod runner;

use pyo3::prelude::*;

use py_charting::PyChartCollection;
use py_framework::{
    PyAccumulativeInsightPcm, PyAlphaModelBase, PyBlackLittermanPcm, PyConfidenceWeightingPcm, PyPortfolioBias,
    PyConstantAlphaModel, PyEmaCrossAlphaModel, PyEqualWeightingPcm, PyExecutionModelBase,
    PyHistoricalReturnsAlphaModel, PyImmediateExecutionModel, PyInsight, PyInsightDirection,
    PyInsightWeightingPcm, PyMacdAlphaModel, PyMaxDrawdownPercentPerSecurity,
    PyMaxDrawdownPercentPortfolio, PyMaxSectorExposureRiskModel, PyMaxSharpeRatioPcm,
    PyMaxUnrealizedProfitPerSecurity, PyMeanReversionPcm, PyMeanVariancePcm,
    PyNullExecutionModel, PyNullRiskManagementModel,
    PyPearsonCorrelationPairsTradingAlphaModel, PyPortfolioConstructionModelBase, PyPortfolioTarget,
    PyRiskManagementModelBase, PyRiskParityPcm, PyRsiAlphaModel, PySpreadExecutionModel,
    PyStandardDeviationExecutionModel, PyTrailingStopRiskModel, PyVwapExecutionModel,
};
use py_data::{
    PyBar, PyCustomData, PyCustomDataPoint, PyDelisting, PyDelistings, PyQuoteBar, PyQuoteBars,
    PySlice, PySymbolChangedEvent, PySymbolChangedEvents, PyTick, PyTicks, PyTradeBar, PyTradeBars,
};
use py_indicators::{
    PyAtr, PyBollingerBands, PyEma, PyIndicatorDataPoint, PyMacd, PyMomp, PyRsi, PySma, PyStd,
};
use py_orders::PyOrderEvent;
use py_portfolio::{PyPortfolio, PySecurityHolding};
use py_qc_algorithm::PyQcAlgorithm;
use py_quant_book::PyQuantBook;
use py_types::{
    PyAlgorithmSettings, PyDataNormalizationMode, PyIndicatorResult, PyMovingAverageType,
    PyOptionSecurity,
    PyResolution, PySecurity, PySecurityEntry, PySecurityManager, PySymbol,
};

// ─── Additional enums matching LEAN's Python API ──────────────────────────────

/// LEAN SecurityType enum values.
#[pyclass(name = "SecurityType", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PySecurityType {
    Base = 0,
    Equity = 1,
    Option = 2,
    Forex = 3,
    Future = 4,
    Cfd = 5,
    Crypto = 7,
    Index = 8,
}

/// LEAN OrderType enum values.
#[pyclass(name = "OrderType", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyOrderType {
    Market = 0,
    Limit = 1,
    StopMarket = 2,
    StopLimit = 3,
    MarketOnClose = 4,
    OptionExercise = 5,
    LimitIfTouched = 6,
    ComboMarket = 7,
    ComboLimit = 8,
    ComboLegLimit = 9,
    TrailingStop = 10,
}

/// LEAN OrderStatus enum values.
#[pyclass(name = "OrderStatus", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyOrderStatus {
    New = 0,
    Submitted = 1,
    PartiallyFilled = 2,
    Filled = 3,
    Canceled = 5,
    Invalid = 6,
    CancelPending = 7,
    UpdateSubmitted = 8,
}

/// LEAN OrderDirection enum values.
#[pyclass(name = "OrderDirection", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyOrderDirection {
    Buy = 0,
    Sell = 1,
    Hold = 2,
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
#[pyo3(name = "AlgorithmImports")]
pub fn algorithm_imports(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Core types
    m.add_class::<PyResolution>()?;
    m.add_class::<PyAlgorithmSettings>()?;
    m.add_class::<PyDataNormalizationMode>()?;
    m.add_class::<PyMovingAverageType>()?;
    m.add_class::<PySymbol>()?;
    m.add_class::<PySecurity>()?;
    m.add_class::<PySecurityEntry>()?;
    m.add_class::<PySecurityManager>()?;
    m.add_class::<PyIndicatorResult>()?;
    m.add_class::<PyIndicatorDataPoint>()?;

    // Data
    m.add_class::<PyTradeBar>()?;
    m.add_class::<PyTradeBars>()?;
    m.add_class::<PyBar>()?;
    m.add_class::<PyQuoteBar>()?;
    m.add_class::<PyQuoteBars>()?;
    m.add_class::<PyTick>()?;
    m.add_class::<PyTicks>()?;
    m.add_class::<PySlice>()?;
    m.add_class::<PyCustomDataPoint>()?;
    m.add_class::<PyCustomData>()?;
    m.add_class::<PyDelisting>()?;
    m.add_class::<PyDelistings>()?;
    m.add_class::<PySymbolChangedEvent>()?;
    m.add_class::<PySymbolChangedEvents>()?;

    // Orders
    m.add_class::<PyOrderEvent>()?;

    // Portfolio
    m.add_class::<PySecurityHolding>()?;
    m.add_class::<PyPortfolio>()?;

    // Options
    m.add_class::<py_options::PyOptionRight>()?;
    m.add_class::<py_options::PyGreeks>()?;
    m.add_class::<py_options::PyOptionContract>()?;
    m.add_class::<py_options::PyUnderlying>()?;
    m.add_class::<py_options::PyOptionChain>()?;
    m.add_class::<py_options::PyOptionChains>()?;
    m.add_class::<PyOptionSecurity>()?;

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
    m.add_class::<PyMomp>()?;
    m.add_class::<PyStd>()?;

    // Additional enums
    m.add_class::<PySecurityType>()?;
    m.add_class::<PyOrderType>()?;
    m.add_class::<PyOrderStatus>()?;
    m.add_class::<PyOrderDirection>()?;

    // ── Insight types ─────────────────────────────────────────────────────────
    m.add_class::<PyInsightDirection>()?;
    m.add_class::<PyInsight>()?;
    m.add_class::<PyPortfolioTarget>()?;

    // ── Algorithm Framework — Base Classes (subclassable) ─────────────────────
    m.add_class::<PyAlphaModelBase>()?;
    m.add_class::<PyPortfolioConstructionModelBase>()?;
    m.add_class::<PyExecutionModelBase>()?;
    m.add_class::<PyRiskManagementModelBase>()?;

    // ── Algorithm Framework — Alpha Models ────────────────────────────────────
    m.add_class::<PyConstantAlphaModel>()?;
    m.add_class::<PyEmaCrossAlphaModel>()?;
    m.add_class::<PyMacdAlphaModel>()?;
    m.add_class::<PyRsiAlphaModel>()?;
    m.add_class::<PyHistoricalReturnsAlphaModel>()?;
    m.add_class::<PyPearsonCorrelationPairsTradingAlphaModel>()?;

    // ── Algorithm Framework — Portfolio Construction Models ───────────────────
    m.add_class::<PyPortfolioBias>()?;
    m.add_class::<PyEqualWeightingPcm>()?;
    m.add_class::<PyInsightWeightingPcm>()?;
    m.add_class::<PyMeanVariancePcm>()?;
    m.add_class::<PyMaxSharpeRatioPcm>()?;
    m.add_class::<PyBlackLittermanPcm>()?;
    m.add_class::<PyRiskParityPcm>()?;
    m.add_class::<PyConfidenceWeightingPcm>()?;
    m.add_class::<PyAccumulativeInsightPcm>()?;
    m.add_class::<PyMeanReversionPcm>()?;

    // ── Algorithm Framework — Execution Models ────────────────────────────────
    m.add_class::<PyImmediateExecutionModel>()?;
    m.add_class::<PyNullExecutionModel>()?;
    m.add_class::<PyVwapExecutionModel>()?;
    m.add_class::<PySpreadExecutionModel>()?;
    m.add_class::<PyStandardDeviationExecutionModel>()?;

    // ── Algorithm Framework — Risk Management Models ──────────────────────────
    m.add_class::<PyNullRiskManagementModel>()?;
    m.add_class::<PyMaxDrawdownPercentPerSecurity>()?;
    m.add_class::<PyTrailingStopRiskModel>()?;
    m.add_class::<PyMaxSectorExposureRiskModel>()?;
    m.add_class::<PyMaxDrawdownPercentPortfolio>()?;
    m.add_class::<PyMaxUnrealizedProfitPerSecurity>()?;

    Ok(())
}

/// Backward compatibility alias — `lean_rust` was the old module name.
/// Keep this so existing internal code that calls `pyo3::append_to_inittab!(lean_rust)`
/// still compiles, though new code should use `AlgorithmImports`.
pub use algorithm_imports as AlgorithmImports;
pub use algorithm_imports as lean_rust;

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(
                result.is_ok(),
                "SimpleMovingAverage creation test failed: {:?}",
                result
            );

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
            assert!(
                result.is_ok(),
                "SMA update(time, value) test failed: {:?}",
                result
            );

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

            // Test 7: All expected LEAN OrderEvent properties are present
            let result = py.run(
                c"
from AlgorithmImports import OrderEvent, OrderStatus, OrderDirection

expected_props = [
    'order_id', 'id', 'symbol', 'utc_time', 'status', 'direction',
    'fill_price', 'fill_price_currency', 'fill_quantity',
    'absolute_fill_quantity', 'quantity', 'is_assignment', 'is_in_the_money',
    'message', 'is_fill', 'order_fee', 'limit_price', 'stop_price',
    'trigger_price', 'trailing_amount', 'trailing_as_percentage',
]
for prop in expected_props:
    assert hasattr(OrderEvent, prop), f'OrderEvent missing property: {prop}'

assert hasattr(OrderStatus, 'New')
assert hasattr(OrderStatus, 'Submitted')
assert hasattr(OrderStatus, 'PartiallyFilled')
assert hasattr(OrderStatus, 'Filled')
assert hasattr(OrderStatus, 'Canceled')

assert hasattr(OrderDirection, 'Buy')
assert hasattr(OrderDirection, 'Sell')
assert hasattr(OrderDirection, 'Hold')
",
                None,
                None,
            );
            assert!(result.is_ok(), "OrderEvent API test failed: {:?}", result);
        });
    }
}
