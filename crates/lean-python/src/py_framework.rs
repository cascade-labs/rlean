/// Algorithm Framework: wires add_alpha / set_portfolio_construction /
/// set_execution / set_risk_management together and runs the pipeline
/// after every on_data call.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use lean_alpha::{IAlphaModel, InsightDirection as AlphaDir};
use lean_core::{Symbol, TimeSpan};
use lean_execution::{
    ExecutionTarget, IExecutionModel, ImmediateExecutionModel, OrderRequest, SecurityData,
};
use lean_portfolio_construction::{
    AccumulativeInsightPortfolioConstructionModel, BlackLittermanOptimizationPortfolioConstructionModel,
    ConfidenceWeightingPortfolioConstructionModel, EqualWeightingPortfolioConstructionModel,
    IPortfolioConstructionModel, InsightDirection as PcmDir, InsightForPcm,
    MeanReversionPortfolioConstructionModel, PortfolioBias, RiskParityPortfolioConstructionModel,
};
use lean_risk::risk_management::{NullRiskManagement, PortfolioTarget as RiskTarget, RiskManagementModel};
use lean_risk::{
    MaximumDrawdownPercentPortfolio, MaximumSectorExposureRiskManagementModel,
    MaximumUnrealizedProfitPercentPerSecurity,
};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

// ─── Framework State ──────────────────────────────────────────────────────────

/// Holds the registered Algorithm Framework models.
/// Shared via Arc<Mutex<>> between PyQcAlgorithm and PyAlgorithmAdapter.
pub struct FrameworkState {
    pub alpha_models: Vec<Box<dyn IAlphaModel>>,
    /// Portfolio construction model — defaults to EqualWeighting.
    pub pcm: Box<dyn IPortfolioConstructionModel>,
    /// Execution model — defaults to Immediate.
    pub exec_model: Box<dyn IExecutionModel>,
    /// Risk management model — defaults to Null (pass-through).
    pub risk_model: Box<dyn RiskManagementModel>,
}

impl FrameworkState {
    pub fn new() -> Self {
        Self {
            alpha_models: Vec::new(),
            pcm: Box::new(EqualWeightingPortfolioConstructionModel::new()),
            exec_model: Box::new(ImmediateExecutionModel::new()),
            risk_model: Box::new(NullRiskManagement),
        }
    }

    /// True when at least one alpha model has been registered.
    pub fn is_active(&self) -> bool {
        !self.alpha_models.is_empty()
    }

    /// Run the full alpha → PCM → risk → execution pipeline.
    ///
    /// Returns a list of order requests that the runner should submit.
    pub fn run_pipeline(
        &mut self,
        slice: &lean_data::Slice,
        securities: &[Symbol],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
        holdings: &HashMap<String, Decimal>,
    ) -> Vec<OrderRequest> {
        // 1. Alpha: gather insights from all registered models.
        let alpha_insights: Vec<lean_alpha::Insight> = self
            .alpha_models
            .iter_mut()
            .flat_map(|m| m.update(slice, securities))
            .collect();

        // Always update PCM price history so warm-up-requiring models (e.g.
        // Black-Litterman) accumulate data even when alpha is silent.
        self.pcm.update_security_prices(prices);

        if alpha_insights.is_empty() {
            return Vec::new();
        }

        // 2. Convert alpha Insights → InsightForPcm.
        let pcm_insights: Vec<InsightForPcm> = alpha_insights
            .iter()
            .map(|i| InsightForPcm {
                symbol: i.symbol.clone(),
                direction: match i.direction {
                    AlphaDir::Up => PcmDir::Up,
                    AlphaDir::Down => PcmDir::Down,
                    AlphaDir::Flat => PcmDir::Flat,
                },
                magnitude: i.magnitude,
                confidence: i.confidence,
                source_model: i.source_model.clone(),
            })
            .collect();

        // 3. Portfolio construction: compute target quantities.
        let pcm_targets = self
            .pcm
            .create_targets(&pcm_insights, portfolio_value, prices);

        // 4. Risk management: filter / adjust targets.
        let risk_targets: Vec<RiskTarget> = pcm_targets
            .iter()
            .map(|t| RiskTarget::new(t.symbol.clone(), t.quantity))
            .collect();
        let adjusted = self.risk_model.manage_risk(&risk_targets);

        // 5. Build SecurityData map for execution model.
        let security_data: HashMap<String, SecurityData> = securities
            .iter()
            .map(|sym| {
                let price = prices.get(&sym.value).copied().unwrap_or(Decimal::ZERO);
                let current_qty = holdings.get(&sym.value).copied().unwrap_or(Decimal::ZERO);
                (
                    sym.value.clone(),
                    SecurityData {
                        symbol: sym.clone(),
                        price,
                        bid: None,
                        ask: None,
                        volume: None,
                        average_volume: None,
                        daily_std_dev: None,
                        current_quantity: current_qty,
                    },
                )
            })
            .collect();

        // 6. Execution: convert targets to concrete order requests.
        let exec_targets: Vec<ExecutionTarget> = adjusted
            .iter()
            .map(|t| ExecutionTarget {
                symbol: t.symbol.clone(),
                quantity: t.quantity,
            })
            .collect();

        self.exec_model.execute(&exec_targets, &security_data)
    }
}

impl Default for FrameworkState {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Python-side helper ───────────────────────────────────────────────────────

/// Run the framework pipeline and return order requests.
/// Called from the runner after on_data (no GIL held).
pub fn run_framework_pipeline(
    framework: &Arc<Mutex<FrameworkState>>,
    alg_inner: &Arc<Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>,
    slice: &lean_data::Slice,
) -> Vec<OrderRequest> {
    {
        let fw = framework.lock().unwrap();
        if !fw.is_active() {
            return Vec::new();
        }
    }

    let (securities, prices, holdings, portfolio_value) = {
        let alg = alg_inner.lock().unwrap();
        let securities: Vec<Symbol> = alg
            .securities
            .all()
            .map(|s| s.symbol.clone())
            .collect();
        let prices: HashMap<String, Decimal> = alg
            .securities
            .all()
            .map(|s| (s.symbol.value.clone(), s.current_price()))
            .collect();
        let holdings: HashMap<String, Decimal> = alg
            .portfolio
            .all_holdings()
            .into_iter()
            .map(|h| (h.symbol.value.clone(), h.quantity))
            .collect();
        let pv = alg.portfolio.total_portfolio_value();
        (securities, prices, holdings, pv)
    };

    let mut fw = framework.lock().unwrap();
    fw.run_pipeline(slice, &securities, portfolio_value, &prices, &holdings)
}

// ─── Python Alpha Model Wrappers ──────────────────────────────────────────────

#[pyclass(name = "ConstantAlphaModel")]
pub struct PyConstantAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyConstantAlphaModel {
    /// direction: "up" | "down" | "flat"; period_days: insight validity in days.
    #[new]
    #[pyo3(signature = (direction, period_days, magnitude=None))]
    pub fn new(direction: &str, period_days: i64, magnitude: Option<f64>) -> Self {
        use lean_alpha::ConstantAlphaModel;
        let dir = match direction.to_lowercase().as_str() {
            "up" => AlphaDir::Up,
            "down" => AlphaDir::Down,
            _ => AlphaDir::Flat,
        };
        let period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        let mag = magnitude.and_then(Decimal::from_f64);
        Self {
            model: Some(Box::new(ConstantAlphaModel {
                direction: dir,
                period,
                magnitude: mag,
            })),
        }
    }
}

#[pyclass(name = "EmaCrossAlphaModel")]
pub struct PyEmaCrossAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyEmaCrossAlphaModel {
    #[new]
    #[pyo3(signature = (fast_period=50, slow_period=200, period_days=1))]
    pub fn new(fast_period: usize, slow_period: usize, period_days: i64) -> Self {
        use lean_alpha::EmaCrossAlphaModel;
        let period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(EmaCrossAlphaModel::new(
                fast_period,
                slow_period,
                period,
            ))),
        }
    }
}

#[pyclass(name = "HistoricalReturnsAlphaModel")]
pub struct PyHistoricalReturnsAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyHistoricalReturnsAlphaModel {
    /// period: lookback in bars (default 1, matching C# default).
    /// insight_period_days: override for insight lifetime in days;
    ///   if None, defaults to period calendar days (matches C# `resolution * lookback`).
    #[new]
    #[pyo3(signature = (period=1, insight_period_days=None))]
    pub fn new(period: usize, insight_period_days: Option<i64>) -> Self {
        use lean_alpha::HistoricalReturnsAlphaModel;
        let days = insight_period_days.unwrap_or(period as i64);
        let insight_period = TimeSpan::from_nanos(days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(HistoricalReturnsAlphaModel::new(period, insight_period))),
        }
    }
}

#[pyclass(name = "PearsonCorrelationPairsTradingAlphaModel")]
pub struct PyPearsonCorrelationPairsTradingAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyPearsonCorrelationPairsTradingAlphaModel {
    /// lookback: number of bars for correlation calculation (C# default: 15).
    /// threshold: % deviation of ratio from EMA mean to trigger signal (C# default: 1.0).
    /// minimum_correlation: minimum Pearson r to consider a pair tradable (C# default: 0.5).
    /// insight_period_days: lifetime of emitted insights in days; defaults to lookback days.
    #[new]
    #[pyo3(signature = (lookback=15, threshold=1.0, minimum_correlation=0.5, insight_period_days=None))]
    pub fn new(
        lookback: usize,
        threshold: f64,
        minimum_correlation: f64,
        insight_period_days: Option<i64>,
    ) -> Self {
        use lean_alpha::PearsonCorrelationPairsTradingAlphaModel;
        let days = insight_period_days.unwrap_or(lookback as i64);
        let insight_period = TimeSpan::from_nanos(days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(PearsonCorrelationPairsTradingAlphaModel::new(
                lookback,
                insight_period,
                threshold,
                minimum_correlation,
            ))),
        }
    }
}

#[pyclass(name = "MacdAlphaModel")]
pub struct PyMacdAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyMacdAlphaModel {
    #[new]
    #[pyo3(signature = (fast_period=12, slow_period=26, signal_period=9, period_days=1))]
    pub fn new(
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        period_days: i64,
    ) -> Self {
        use lean_alpha::MacdAlphaModel;
        let period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(MacdAlphaModel::new(
                fast_period,
                slow_period,
                signal_period,
                period,
            ))),
        }
    }
}

#[pyclass(name = "RsiAlphaModel")]
pub struct PyRsiAlphaModel {
    pub model: Option<Box<dyn IAlphaModel>>,
}

#[pymethods]
impl PyRsiAlphaModel {
    #[new]
    #[pyo3(signature = (period=14, period_days=1))]
    pub fn new(period: usize, period_days: i64) -> Self {
        use lean_alpha::RsiAlphaModel;
        let insight_period = TimeSpan::from_nanos(period_days * 86_400 * 1_000_000_000);
        Self {
            model: Some(Box::new(RsiAlphaModel::new(period, insight_period))),
        }
    }
}

// ─── PortfolioBias ────────────────────────────────────────────────────────────

/// LEAN PortfolioBias — controls whether the PCM may take short positions.
#[pyclass(name = "PortfolioBias", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyPortfolioBias {
    Short = -1,
    LongShort = 0,
    Long = 1,
}

impl From<PyPortfolioBias> for PortfolioBias {
    fn from(b: PyPortfolioBias) -> Self {
        match b {
            PyPortfolioBias::Short => PortfolioBias::Short,
            PyPortfolioBias::LongShort => PortfolioBias::LongShort,
            PyPortfolioBias::Long => PortfolioBias::Long,
        }
    }
}

// ─── Python PCM Wrappers ──────────────────────────────────────────────────────

#[pyclass(name = "EqualWeightingPortfolioConstructionModel")]
pub struct PyEqualWeightingPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyEqualWeightingPcm {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(EqualWeightingPortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyEqualWeightingPcm {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "InsightWeightingPortfolioConstructionModel")]
pub struct PyInsightWeightingPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyInsightWeightingPcm {
    #[new]
    pub fn new() -> Self {
        use lean_portfolio_construction::InsightWeightingPortfolioConstructionModel;
        Self {
            model: Some(Box::new(InsightWeightingPortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyInsightWeightingPcm {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "MeanVarianceOptimizationPortfolioConstructionModel")]
pub struct PyMeanVariancePcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyMeanVariancePcm {
    #[new]
    pub fn new() -> Self {
        use lean_portfolio_construction::MeanVariancePortfolioConstructionModel;
        Self {
            model: Some(Box::new(MeanVariancePortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyMeanVariancePcm {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "MaximumSharpeRatioPortfolioConstructionModel")]
pub struct PyMaxSharpeRatioPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyMaxSharpeRatioPcm {
    #[new]
    pub fn new() -> Self {
        use lean_portfolio_construction::MaximumSharpeRatioPortfolioConstructionModel;
        Self {
            model: Some(Box::new(
                MaximumSharpeRatioPortfolioConstructionModel::new(),
            )),
        }
    }
}

impl Default for PyMaxSharpeRatioPcm {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "BlackLittermanOptimizationPortfolioConstructionModel")]
pub struct PyBlackLittermanPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyBlackLittermanPcm {
    /// Create a Black-Litterman PCM.  Matches C# LEAN parameter names:
    ///
    /// ```python
    /// BlackLittermanOptimizationPortfolioConstructionModel(
    ///     rebalance=timedelta(days=7),        # accepted, currently ignored
    ///     portfolio_bias=PortfolioBias.Long,
    ///     lookback=1,
    ///     period=63,
    ///     resolution=Resolution.Daily,        # accepted, currently ignored
    ///     risk_free_rate=0.0,
    ///     delta=2.5,
    ///     tau=0.05,
    /// )
    /// ```
    #[new]
    #[allow(clippy::too_many_arguments)]
    #[pyo3(signature = (
        rebalance=None,
        portfolio_bias=PyPortfolioBias::LongShort,
        lookback=1,
        period=63,
        resolution=None,
        risk_free_rate=0.0,
        delta=2.5,
        tau=0.05,
    ))]
    pub fn new(
        rebalance: Option<&Bound<'_, PyAny>>,
        portfolio_bias: PyPortfolioBias,
        lookback: usize,
        period: usize,
        resolution: Option<&Bound<'_, PyAny>>,
        risk_free_rate: f64,
        delta: f64,
        tau: f64,
    ) -> Self {
        let _ = rebalance;   // rebalancing frequency not yet implemented in rlean
        let _ = resolution;  // resolution hint accepted but unused
        Self {
            model: Some(Box::new(
                BlackLittermanOptimizationPortfolioConstructionModel::with_params(
                    lookback,
                    period,
                    risk_free_rate,
                    delta,
                    tau,
                    portfolio_bias.into(),
                ),
            )),
        }
    }
}

#[pyclass(name = "RiskParityPortfolioConstructionModel")]
pub struct PyRiskParityPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyRiskParityPcm {
    /// Create a Risk Parity PCM.
    ///
    /// Parameters (all optional, match C# defaults):
    /// - lookback: ROC period in bars (default 1)
    /// - period: rolling window length for covariance estimation (default 252)
    #[new]
    #[pyo3(signature = (lookback=1, period=252))]
    pub fn new(lookback: usize, period: usize) -> Self {
        Self {
            model: Some(Box::new(RiskParityPortfolioConstructionModel::with_params(
                lookback, period,
            ))),
        }
    }
}

#[pyclass(name = "ConfidenceWeightedPortfolioConstructionModel")]
pub struct PyConfidenceWeightingPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyConfidenceWeightingPcm {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(ConfidenceWeightingPortfolioConstructionModel::new())),
        }
    }
}

impl Default for PyConfidenceWeightingPcm {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "AccumulativeInsightPortfolioConstructionModel")]
pub struct PyAccumulativeInsightPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyAccumulativeInsightPcm {
    /// Create an AccumulativeInsight PCM.
    ///
    /// `percent`: per-insight allocation fraction (default 0.03 = 3 %).
    #[new]
    #[pyo3(signature = (percent=0.03))]
    pub fn new(percent: f64) -> Self {
        use rust_decimal::prelude::FromPrimitive;
        let pct = Decimal::from_f64(percent).unwrap_or(Decimal::new(3, 2));
        Self {
            model: Some(Box::new(
                AccumulativeInsightPortfolioConstructionModel::with_percent(pct),
            )),
        }
    }
}

#[pyclass(name = "MeanReversionPortfolioConstructionModel")]
pub struct PyMeanReversionPcm {
    pub model: Option<Box<dyn IPortfolioConstructionModel>>,
}

#[pymethods]
impl PyMeanReversionPcm {
    /// Create a Mean Reversion (OLMAR) PCM.
    ///
    /// Parameters (match C# defaults):
    /// - `reversion_threshold`: ε (default 1.0)
    /// - `window_size`: SMA period (default 20)
    #[new]
    #[pyo3(signature = (reversion_threshold=1.0, window_size=20))]
    pub fn new(reversion_threshold: f64, window_size: usize) -> Self {
        Self {
            model: Some(Box::new(MeanReversionPortfolioConstructionModel::with_params(
                reversion_threshold,
                window_size,
            ))),
        }
    }
}

// ─── Python Execution Model Wrappers ─────────────────────────────────────────

#[pyclass(name = "ImmediateExecutionModel")]
pub struct PyImmediateExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PyImmediateExecutionModel {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(ImmediateExecutionModel::new())),
        }
    }
}

#[pyclass(name = "NullExecutionModel")]
pub struct PyNullExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PyNullExecutionModel {
    #[new]
    pub fn new() -> Self {
        use lean_execution::NullExecutionModel;
        Self {
            model: Some(Box::new(NullExecutionModel::new())),
        }
    }
}

impl Default for PyImmediateExecutionModel {
    fn default() -> Self { Self::new() }
}

impl Default for PyNullExecutionModel {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "VolumeWeightedAveragePriceExecutionModel")]
pub struct PyVwapExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PyVwapExecutionModel {
    #[new]
    #[pyo3(signature = (participation_rate=0.2))]
    pub fn new(participation_rate: f64) -> Self {
        use lean_execution::VwapExecutionModel;
        let rate = Decimal::from_f64(participation_rate).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(VwapExecutionModel::new(rate))),
        }
    }
}

#[pyclass(name = "SpreadExecutionModel")]
pub struct PySpreadExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PySpreadExecutionModel {
    /// accepting_spread_percent: maximum spread as a fraction of price (default 0.5% = 0.005).
    /// Mirrors C# SpreadExecutionModel(decimal acceptingSpreadPercent = 0.005m).
    #[new]
    #[pyo3(signature = (accepting_spread_percent=0.005))]
    pub fn new(accepting_spread_percent: f64) -> Self {
        use lean_execution::SpreadExecutionModel;
        let pct = Decimal::from_f64(accepting_spread_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(SpreadExecutionModel::new(pct))),
        }
    }
}

#[pyclass(name = "StandardDeviationExecutionModel")]
pub struct PyStandardDeviationExecutionModel {
    pub model: Option<Box<dyn IExecutionModel>>,
}

#[pymethods]
impl PyStandardDeviationExecutionModel {
    /// deviations: number of std deviations from the mean required to trigger execution (default 2.0).
    /// Mirrors C# StandardDeviationExecutionModel(int period = 60, decimal deviations = 2m, ...).
    /// Note: period is accepted for API compatibility but the Rust model uses security.daily_std_dev
    /// directly rather than maintaining a rolling indicator, so period is unused.
    #[new]
    #[pyo3(signature = (period=60, deviations=2.0))]
    pub fn new(period: i64, deviations: f64) -> Self {
        use lean_execution::StandardDeviationExecutionModel;
        let _ = period; // API-compatible; Rust model uses pre-computed daily_std_dev
        let devs = Decimal::from_f64(deviations).unwrap_or(Decimal::from(2));
        Self {
            model: Some(Box::new(StandardDeviationExecutionModel::new(devs))),
        }
    }
}

// ─── Python Risk Model Wrappers ───────────────────────────────────────────────

#[pyclass(name = "NullRiskManagementModel")]
pub struct PyNullRiskManagementModel {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyNullRiskManagementModel {
    #[new]
    pub fn new() -> Self {
        Self {
            model: Some(Box::new(NullRiskManagement)),
        }
    }
}

impl Default for PyNullRiskManagementModel {
    fn default() -> Self { Self::new() }
}

#[pyclass(name = "MaximumDrawdownPercentPerSecurity")]
pub struct PyMaxDrawdownPercentPerSecurity {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxDrawdownPercentPerSecurity {
    #[new]
    #[pyo3(signature = (maximum_drawdown_percent=0.05))]
    pub fn new(maximum_drawdown_percent: f64) -> Self {
        use lean_risk::MaximumDrawdownPercentPerSecurity;
        let pct = Decimal::from_f64(maximum_drawdown_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumDrawdownPercentPerSecurity::new(pct))),
        }
    }
}

#[pyclass(name = "TrailingStopRiskManagementModel")]
pub struct PyTrailingStopRiskModel {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyTrailingStopRiskModel {
    #[new]
    #[pyo3(signature = (trailing_amount=0.05))]
    pub fn new(trailing_amount: f64) -> Self {
        use lean_risk::TrailingStopRiskManagementModel;
        let pct = Decimal::from_f64(trailing_amount).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(TrailingStopRiskManagementModel::new(pct))),
        }
    }
}

#[pyclass(name = "MaximumSectorExposureRiskManagementModel")]
pub struct PyMaxSectorExposureRiskModel {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxSectorExposureRiskModel {
    #[new]
    #[pyo3(signature = (maximum_sector_exposure=0.20))]
    pub fn new(maximum_sector_exposure: f64) -> Self {
        let pct = Decimal::from_f64(maximum_sector_exposure).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumSectorExposureRiskManagementModel::new(pct))),
        }
    }
}

#[pyclass(name = "MaximumDrawdownPercentPortfolio")]
pub struct PyMaxDrawdownPercentPortfolio {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxDrawdownPercentPortfolio {
    #[new]
    #[pyo3(signature = (maximum_drawdown_percent=0.05, is_trailing=false))]
    pub fn new(maximum_drawdown_percent: f64, is_trailing: bool) -> Self {
        let pct = Decimal::from_f64(maximum_drawdown_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumDrawdownPercentPortfolio::new(
                pct,
                is_trailing,
            ))),
        }
    }
}

#[pyclass(name = "MaximumUnrealizedProfitPercentPerSecurity")]
pub struct PyMaxUnrealizedProfitPerSecurity {
    pub model: Option<Box<dyn RiskManagementModel>>,
}

#[pymethods]
impl PyMaxUnrealizedProfitPerSecurity {
    #[new]
    #[pyo3(signature = (maximum_unrealized_profit_percent=0.05))]
    pub fn new(maximum_unrealized_profit_percent: f64) -> Self {
        let pct = Decimal::from_f64(maximum_unrealized_profit_percent).unwrap_or(Decimal::ZERO);
        Self {
            model: Some(Box::new(MaximumUnrealizedProfitPercentPerSecurity::new(pct))),
        }
    }
}

// ─── Insight types exposed to Python ─────────────────────────────────────────

#[pyclass(name = "InsightDirection", eq, eq_int)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PyInsightDirection {
    Up = 1,
    Flat = 0,
    Down = -1,
}

/// A single alpha signal.  Mirrors C# `Insight`.
#[pyclass(name = "Insight")]
pub struct PyInsight {
    pub symbol: lean_core::Symbol,
    pub direction: PyInsightDirection,
    /// Insight validity in nanoseconds.
    pub period_nanos: i64,
    pub magnitude: Option<f64>,
    pub confidence: Option<f64>,
    pub source_model: String,
}

#[pymethods]
impl PyInsight {
    /// ``Insight(symbol, InsightDirection.Up, timedelta(days=1))``
    #[new]
    #[pyo3(signature = (symbol, direction, period, magnitude=None, confidence=None, source_model=None))]
    pub fn new(
        symbol: &Bound<'_, PyAny>,
        direction: PyInsightDirection,
        period: &Bound<'_, PyAny>,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
    ) -> PyResult<Self> {
        use crate::py_types::PySymbol;
        let lean_symbol = if let Ok(s) = symbol.downcast::<PySymbol>() {
            s.borrow().inner.clone()
        } else {
            let s: String = symbol.extract()?;
            lean_core::Symbol::create_equity(&s, &lean_core::Market::usa())
        };

        // period can be datetime.timedelta or a plain float/int (interpreted as days).
        let nanos: i64 = if let Ok(days_float) = period.extract::<f64>() {
            (days_float * 86_400.0 * 1_000_000_000.0) as i64
        } else {
            let days: f64 = period.getattr("days")?.extract()?;
            let secs: f64 = period.getattr("seconds")?.extract().unwrap_or(0.0);
            let micros: f64 = period.getattr("microseconds")?.extract().unwrap_or(0.0);
            ((days * 86_400.0 + secs + micros / 1_000_000.0) * 1_000_000_000.0) as i64
        };

        Ok(Self {
            symbol: lean_symbol,
            direction,
            period_nanos: nanos,
            magnitude,
            confidence,
            source_model: source_model.unwrap_or_default(),
        })
    }

    /// ``Insight.price(symbol, timedelta(days=1), InsightDirection.Up)``  (snake_case)
    #[staticmethod]
    #[pyo3(signature = (symbol, period, direction, magnitude=None, confidence=None, source_model=None, weight=None))]
    pub fn price(
        symbol: &Bound<'_, PyAny>,
        period: &Bound<'_, PyAny>,
        direction: PyInsightDirection,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
        weight: Option<f64>,
    ) -> PyResult<Self> {
        let _ = weight;
        Self::new(symbol, direction, period, magnitude, confidence, source_model)
    }

    /// ``Insight.Price(symbol, timedelta(days=1), InsightDirection.Up)``  (LEAN PascalCase)
    #[staticmethod]
    #[pyo3(name = "Price", signature = (symbol, period, direction, magnitude=None, confidence=None, source_model=None, weight=None))]
    #[allow(non_snake_case)]
    pub fn Price(
        symbol: &Bound<'_, PyAny>,
        period: &Bound<'_, PyAny>,
        direction: PyInsightDirection,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
        weight: Option<f64>,
    ) -> PyResult<Self> {
        let _ = weight; // LEAN API compat — not used in rlean
        Self::new(symbol, direction, period, magnitude, confidence, source_model)
    }

    #[getter]
    pub fn symbol(&self) -> crate::py_types::PySymbol {
        crate::py_types::PySymbol { inner: self.symbol.clone() }
    }

    #[getter]
    pub fn direction(&self) -> PyInsightDirection {
        self.direction
    }

    #[getter]
    pub fn magnitude(&self) -> Option<f64> {
        self.magnitude
    }

    #[getter]
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }

    #[getter]
    pub fn source_model(&self) -> &str {
        &self.source_model
    }

    /// Forward LEAN PascalCase attribute access to snake_case equivalents.
    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<PyObject> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'Insight' object has no attribute '{name}'"
        )))
    }
}

// ─── PortfolioTarget Python class ────────────────────────────────────────────

/// LEAN API: ``PortfolioTarget(symbol, quantity)`` / ``PortfolioTarget.Percent(algorithm, symbol, pct)``
/// Returned by Python PCM ``CreateTargets`` implementations.
#[pyclass(name = "PortfolioTarget")]
pub struct PyPortfolioTarget {
    pub symbol: lean_core::Symbol,
    /// Absolute quantity (if constructed directly).
    pub quantity: Option<f64>,
    /// Target percent of portfolio (e.g. 0.10 = 10%).
    pub percent: Option<f64>,
}

#[pymethods]
impl PyPortfolioTarget {
    #[new]
    pub fn new(symbol: &Bound<'_, PyAny>, quantity: f64) -> PyResult<Self> {
        let lean_symbol = extract_lean_symbol_from_py(symbol)?;
        Ok(Self { symbol: lean_symbol, quantity: Some(quantity), percent: None })
    }

    /// ``PortfolioTarget.Percent(algorithm, symbol, percent)`` — deferred percent target.
    #[staticmethod]
    #[pyo3(name = "Percent")]
    #[allow(non_snake_case)]
    pub fn Percent(
        _algorithm: &Bound<'_, PyAny>,
        symbol: &Bound<'_, PyAny>,
        percent: f64,
    ) -> PyResult<Self> {
        let lean_symbol = extract_lean_symbol_from_py(symbol)?;
        Ok(Self { symbol: lean_symbol, quantity: None, percent: Some(percent) })
    }

    /// snake_case alias.
    #[staticmethod]
    pub fn percent(
        algorithm: &Bound<'_, PyAny>,
        symbol: &Bound<'_, PyAny>,
        pct: f64,
    ) -> PyResult<Self> {
        Self::Percent(algorithm, symbol, pct)
    }

    #[getter]
    fn symbol(&self) -> crate::py_types::PySymbol {
        crate::py_types::PySymbol { inner: self.symbol.clone() }
    }

    fn __repr__(&self) -> String {
        if let Some(pct) = self.percent {
            format!("PortfolioTarget('{}', {:.1}%)", self.symbol.value, pct * 100.0)
        } else {
            format!(
                "PortfolioTarget('{}', qty={:.0})",
                self.symbol.value,
                self.quantity.unwrap_or(0.0)
            )
        }
    }
}

fn extract_lean_symbol_from_py(symbol: &Bound<'_, PyAny>) -> PyResult<lean_core::Symbol> {
    if let Ok(s) = symbol.downcast::<crate::py_types::PySymbol>() {
        return Ok(s.borrow().inner.clone());
    }
    let ticker: String = symbol.extract()?;
    Ok(lean_core::Symbol::create_equity(&ticker, &lean_core::Market::usa()))
}

// ─── Framework Base Classes (subclassable by Python strategies) ───────────────

/// Base class for Python-defined alpha models.
///
/// ```python
/// from AlgorithmImports import *
///
/// class MyAlpha(AlphaModel):
///     def update(self, algorithm, data):
///         # return a list of Insight objects
///         return [Insight(symbol, InsightDirection.Up, timedelta(days=1))
///                 for symbol in [self.spy]]
/// ```
#[pyclass(name = "AlphaModel", subclass)]
pub struct PyAlphaModelBase;

#[pymethods]
impl PyAlphaModelBase {
    /// Accept any constructor arguments so Python subclasses can add their own __init__.
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }

    /// Override in subclass — return a list of `Insight` objects.
    /// Matches LEAN's PascalCase API: ``def Update(self, algorithm, data): ...``
    #[pyo3(name = "Update")]
    #[allow(non_snake_case)]
    fn Update(
        &self,
        _algorithm: &Bound<'_, PyAny>,
        _data: &Bound<'_, PyAny>,
    ) -> Vec<PyObject> {
        vec![]
    }

    #[pyo3(name = "OnSecuritiesChanged")]
    #[allow(non_snake_case)]
    fn OnSecuritiesChanged(
        &self,
        _algorithm: &Bound<'_, PyAny>,
        _changes: &Bound<'_, PyAny>,
    ) {
    }
}

/// Base class for Python-defined portfolio construction models.
#[pyclass(name = "PortfolioConstructionModel", subclass)]
pub struct PyPortfolioConstructionModelBase;

#[pymethods]
impl PyPortfolioConstructionModelBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
}

/// Base class for Python-defined execution models.
#[pyclass(name = "ExecutionModel", subclass)]
pub struct PyExecutionModelBase;

#[pymethods]
impl PyExecutionModelBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
}

/// Base class for Python-defined risk management models.
#[pyclass(name = "RiskManagementModel", subclass)]
pub struct PyRiskManagementModelBase;

#[pymethods]
impl PyRiskManagementModelBase {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    pub fn new(_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>) -> Self {
        Self
    }
}

// ─── Python-defined alpha model adapter ──────────────────────────────────────

/// Wraps a Python object that subclasses `AlphaModel` so it can be used as a
/// Rust `IAlphaModel`.  Calls the Python `Update(algorithm, data)` method on
/// each bar, passing the real `QCAlgorithm` instance and a `Slice` proxy.
struct PyAlphaAdapter {
    /// The Python AlphaModel subclass instance.
    obj: Py<PyAny>,
    /// The Python QCAlgorithm instance — passed as `algorithm` to `Update()`.
    alg_py: Py<PyAny>,
}

impl IAlphaModel for PyAlphaAdapter {
    fn update(
        &mut self,
        slice: &lean_data::Slice,
        _securities: &[lean_core::Symbol],
    ) -> Vec<lean_alpha::Insight> {
        Python::with_gil(|py| {
            // Build a PySlice proxy to pass as `data`.
            let slice_py: PyObject = match crate::py_data::PySlice::from_slice(py, slice) {
                Ok(s) => match Py::new(py, s) {
                    Ok(p) => p.into_any(),
                    Err(e) => {
                        tracing::warn!("PyAlphaAdapter: PySlice alloc error: {e}");
                        py.None()
                    }
                },
                Err(e) => {
                    tracing::warn!("PyAlphaAdapter: from_slice error: {e}");
                    py.None()
                }
            };

            let alg = self.alg_py.bind(py);
            let result = self.obj.call_method1(py, "Update", (alg, slice_py));
            match result {
                Ok(list_obj) => extract_py_insights(py, list_obj.bind(py)),
                Err(e) => {
                    tracing::warn!("PyAlphaAdapter::Update error: {e}");
                    vec![]
                }
            }
        })
    }

    fn on_securities_changed(&mut self, _added: &[lean_core::Symbol], _removed: &[lean_core::Symbol]) {}
}

// ─── Python-defined PCM adapter ──────────────────────────────────────────────

/// Wraps a Python object that subclasses ``PortfolioConstructionModel`` so it
/// can be used as a Rust ``IPortfolioConstructionModel``.
/// Calls ``CreateTargets(algorithm, insights)`` in Python.
struct PyPcmAdapter {
    obj: Py<PyAny>,
    alg_py: Py<PyAny>,
}

impl IPortfolioConstructionModel for PyPcmAdapter {
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<lean_portfolio_construction::PortfolioTarget> {
        Python::with_gil(|py| {
            // Build PyInsight list to pass as the `insights` argument.
            let py_insights: Vec<PyObject> = insights
                .iter()
                .map(|i| {
                    let direction = match i.direction {
                        PcmDir::Up => PyInsightDirection::Up,
                        PcmDir::Down => PyInsightDirection::Down,
                        PcmDir::Flat => PyInsightDirection::Flat,
                    };
                    let pi = PyInsight {
                        symbol: i.symbol.clone(),
                        direction,
                        period_nanos: 86_400 * 1_000_000_000,
                        magnitude: i.magnitude.and_then(|m| m.to_f64()),
                        confidence: i.confidence.and_then(|c| c.to_f64()),
                        source_model: i.source_model.clone(),
                    };
                    Py::new(py, pi)
                        .map(|p| p.into_any())
                        .unwrap_or_else(|_| py.None())
                })
                .collect();

            let alg = self.alg_py.bind(py);
            let list = pyo3::types::PyList::new(py, py_insights).unwrap();

            match self.obj.call_method1(py, "CreateTargets", (alg, list)) {
                Ok(result) => {
                    extract_pcm_targets(py, result.bind(py), portfolio_value, prices)
                }
                Err(e) => {
                    tracing::warn!("PyPcmAdapter::CreateTargets error: {e}");
                    vec![]
                }
            }
        })
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}

/// Convert a Python iterable of ``PortfolioTarget`` objects into Rust ``PortfolioTarget``s.
fn extract_pcm_targets(
    _py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    portfolio_value: Decimal,
    prices: &HashMap<String, Decimal>,
) -> Vec<lean_portfolio_construction::PortfolioTarget> {
    let Ok(iter) = obj.try_iter() else { return vec![] };
    iter.filter_map(|item| {
        let item = item.ok()?;

        // Fast path: typed PyPortfolioTarget.
        if let Ok(pt) = item.downcast::<PyPortfolioTarget>() {
            let pt = pt.borrow();
            let sym = pt.symbol.clone();
            if let Some(pct) = pt.percent {
                let pct_dec = Decimal::from_f64(pct)?;
                let price = prices.get(&sym.value).copied().unwrap_or(Decimal::ONE);
                return Some(lean_portfolio_construction::PortfolioTarget::percent(
                    sym, pct_dec, portfolio_value, price,
                ));
            }
            if let Some(qty) = pt.quantity {
                return Some(lean_portfolio_construction::PortfolioTarget::new(
                    sym,
                    Decimal::from_f64(qty)?,
                ));
            }
            return None;
        }

        // Duck-type fallback: any object with .symbol and .percent / .quantity attrs.
        let sym_attr = item.getattr("symbol").ok()?;
        let sym = if let Ok(s) = sym_attr.downcast::<crate::py_types::PySymbol>() {
            s.borrow().inner.clone()
        } else {
            let ticker: String = sym_attr
                .extract()
                .or_else(|_| sym_attr.str().map(|s| s.to_string()))
                .ok()?;
            lean_core::Symbol::create_equity(&ticker, &lean_core::Market::usa())
        };

        if let Ok(pct_attr) = item.getattr("percent").or_else(|_| item.getattr("Percent")) {
            if let Ok(pct) = pct_attr.extract::<f64>() {
                let pct_dec = Decimal::from_f64(pct)?;
                let price = prices.get(&sym.value).copied().unwrap_or(Decimal::ONE);
                return Some(lean_portfolio_construction::PortfolioTarget::percent(
                    sym, pct_dec, portfolio_value, price,
                ));
            }
        }
        if let Ok(qty_attr) = item.getattr("quantity").or_else(|_| item.getattr("Quantity")) {
            if let Ok(qty) = qty_attr.extract::<f64>() {
                return Some(lean_portfolio_construction::PortfolioTarget::new(
                    sym,
                    Decimal::from_f64(qty)?,
                ));
            }
        }
        None
    })
    .collect()
}

/// Convert a Python iterable of `Insight` objects into Rust `Insight`s.
fn extract_py_insights(_py: Python<'_>, obj: &Bound<'_, PyAny>) -> Vec<lean_alpha::Insight> {
    let Ok(iter) = obj.try_iter() else {
        return vec![];
    };
    let mut out = Vec::new();
    for item in iter {
        let Ok(item) = item else { continue };
        if let Ok(pi) = item.downcast::<PyInsight>() {
            let pi = pi.borrow();
            let period = lean_core::TimeSpan::from_nanos(pi.period_nanos);
            let dir = match pi.direction {
                PyInsightDirection::Up => lean_alpha::InsightDirection::Up,
                PyInsightDirection::Down => lean_alpha::InsightDirection::Down,
                PyInsightDirection::Flat => lean_alpha::InsightDirection::Flat,
            };
            let mag = pi.magnitude.and_then(Decimal::from_f64);
            let conf = pi.confidence.and_then(Decimal::from_f64);
            out.push(lean_alpha::Insight::new(
                pi.symbol.clone(),
                dir,
                period,
                mag,
                conf,
                &pi.source_model,
            ));
        } else {
            // Attempt duck-type extraction: symbol str + direction int + period timedelta
            let sym_obj = item.getattr("symbol").ok();
            let dir_obj = item.getattr("direction").ok();
            let per_obj = item.getattr("period").ok();
            if let (Some(sym), Some(dir_o), Some(per)) = (sym_obj, dir_obj, per_obj) {
                let symbol_str: String = sym
                    .extract()
                    .or_else(|_| sym.str().map(|s| s.to_string()))
                    .unwrap_or_default();
                if symbol_str.is_empty() {
                    continue;
                }
                let symbol = lean_core::Symbol::create_equity(&symbol_str, &lean_core::Market::usa());
                let dir_val: i32 = dir_o.extract().unwrap_or(1);
                let dir = if dir_val > 0 {
                    lean_alpha::InsightDirection::Up
                } else if dir_val < 0 {
                    lean_alpha::InsightDirection::Down
                } else {
                    lean_alpha::InsightDirection::Flat
                };
                let days: f64 = per
                    .getattr("days")
                    .and_then(|d| d.extract())
                    .unwrap_or(1.0);
                let period =
                    lean_core::TimeSpan::from_nanos((days * 86_400.0 * 1_000_000_000.0) as i64);
                out.push(lean_alpha::Insight::new(
                    symbol, dir, period, None, None, "",
                ));
            }
        }
    }
    out
}

// ─── Extraction helpers ───────────────────────────────────────────────────────

/// Try to extract an IAlphaModel Box from a Python object.
/// `alg_py` is the Python QCAlgorithm instance — passed as `algorithm` to `Update()`.
pub fn try_take_alpha(model: &Bound<'_, PyAny>, alg_py: Py<PyAny>) -> Option<Box<dyn IAlphaModel>> {
    if let Ok(m) = model.downcast::<PyEmaCrossAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMacdAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyRsiAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyConstantAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyHistoricalReturnsAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyPearsonCorrelationPairsTradingAlphaModel>() {
        return m.borrow_mut().model.take();
    }
    // Accept any Python object that subclasses AlphaModel.
    if model.is_instance_of::<PyAlphaModelBase>() {
        return Some(Box::new(PyAlphaAdapter {
            obj: model.clone().unbind(),
            alg_py,
        }));
    }
    tracing::warn!("add_alpha: unrecognized model type — use a built-in AlphaModel class");
    None
}

/// Try to extract an IPortfolioConstructionModel Box from a Python object.
pub fn try_take_pcm(
    model: &Bound<'_, PyAny>,
    alg_py: Py<PyAny>,
) -> Option<Box<dyn IPortfolioConstructionModel>> {
    if let Ok(m) = model.downcast::<PyEqualWeightingPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyInsightWeightingPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMeanVariancePcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMaxSharpeRatioPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyBlackLittermanPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyRiskParityPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyConfidenceWeightingPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyAccumulativeInsightPcm>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMeanReversionPcm>() {
        return m.borrow_mut().model.take();
    }
    // Accept any Python object that subclasses PortfolioConstructionModel.
    if model.is_instance_of::<PyPortfolioConstructionModelBase>() {
        return Some(Box::new(PyPcmAdapter {
            obj: model.clone().unbind(),
            alg_py,
        }));
    }
    tracing::warn!(
        "set_portfolio_construction: unrecognized model type — subclass PortfolioConstructionModel or use a built-in PCM"
    );
    None
}

/// Try to extract an IExecutionModel Box from a Python object.
pub fn try_take_exec(model: &Bound<'_, PyAny>) -> Option<Box<dyn IExecutionModel>> {
    if let Ok(m) = model.downcast::<PyImmediateExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyNullExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyVwapExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PySpreadExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyStandardDeviationExecutionModel>() {
        return m.borrow_mut().model.take();
    }
    tracing::warn!("set_execution: unrecognized model type — use a built-in ExecutionModel class");
    None
}

/// Try to extract a RiskManagementModel Box from a Python object.
pub fn try_take_risk(model: &Bound<'_, PyAny>) -> Option<Box<dyn RiskManagementModel>> {
    if let Ok(m) = model.downcast::<PyNullRiskManagementModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMaxDrawdownPercentPerSecurity>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyTrailingStopRiskModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMaxSectorExposureRiskModel>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMaxDrawdownPercentPortfolio>() {
        return m.borrow_mut().model.take();
    }
    if let Ok(m) = model.downcast::<PyMaxUnrealizedProfitPerSecurity>() {
        return m.borrow_mut().model.take();
    }
    tracing::warn!(
        "set_risk_management: unrecognized model type — use a built-in RiskManagementModel class"
    );
    None
}
