//! SDK-owned framework model and pipeline APIs.

use std::collections::HashMap;

use rlean_alpha::{IAlphaModel, Insight, InsightDirection as AlphaInsightDirection};
use rlean_core::{DateTime, Resolution, Symbol, TimeSpan};
use rlean_execution::{
    AdaptiveMakerTakerExecutionModel, AggressivePostOnlyExecutionModel, ExecutionTarget,
    IExecutionModel, ImmediateExecutionModel as LeanImmediateExecutionModel,
    MakerThenTakerExecutionModel,
};
use rlean_portfolio_construction::{
    AccumulativeInsightPortfolioConstructionModel,
    BlackLittermanOptimizationPortfolioConstructionModel,
    ConfidenceWeightingPortfolioConstructionModel,
    EqualWeightingPortfolioConstructionModel as LeanEqualWeightingPortfolioConstructionModel,
    IPortfolioConstructionModel, InsightDirection as PcmInsightDirection, InsightForPcm,
    MeanReversionPortfolioConstructionModel, PortfolioBias, PortfolioTarget, RebalancePolicy,
    RiskParityPortfolioConstructionModel,
};
use rlean_risk::risk_management::{
    NullRiskManagement, PortfolioTarget as RiskPortfolioTarget,
    RiskManagementModel as RiskManagementModelTrait,
};
use rlean_risk::{
    MaximumDrawdownPercentPortfolio, MaximumSectorExposureRiskManagementModel,
    MaximumUnrealizedProfitPercentPerSecurity,
};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;

pub fn days_to_timespan(days: i64) -> TimeSpan {
    TimeSpan::from_nanos(days * 86_400 * 1_000_000_000)
}

pub fn resolution_rebalance_period(resolution: Resolution) -> Option<TimeSpan> {
    resolution.to_time_span()
}

pub fn seconds_rebalance_period(seconds: f64) -> Option<TimeSpan> {
    if seconds.is_finite() && seconds > 0.0 {
        Some(TimeSpan::from_nanos((seconds * 1_000_000_000.0) as i64))
    } else {
        None
    }
}

pub fn default_rebalance_period() -> Option<TimeSpan> {
    Some(TimeSpan::ONE_DAY)
}

pub fn default_rebalance_policy() -> RebalancePolicy {
    RebalancePolicy::default()
}

pub fn period_rebalance_policy(period: TimeSpan) -> RebalancePolicy {
    RebalancePolicy::period(period)
}

pub fn every_slice_rebalance_policy() -> RebalancePolicy {
    RebalancePolicy::every_slice()
}

pub fn rebalance_policy_from_period(period: Option<TimeSpan>) -> RebalancePolicy {
    RebalancePolicy::from_period(period)
}

pub fn insight_changes_only_rebalance_policy() -> RebalancePolicy {
    RebalancePolicy::insight_changes_only()
}

pub fn portfolio_bias_from_view(bias: PortfolioBiasView) -> PortfolioBias {
    match bias {
        PortfolioBiasView::LongShort => PortfolioBias::LongShort,
        PortfolioBiasView::Long => PortfolioBias::Long,
        PortfolioBiasView::Short => PortfolioBias::Short,
    }
}

/// Parses a Python-side `rebalance` argument, which LEAN allows to be a
/// `Resolution`, a `datetime.timedelta`, a duration expressed in seconds, or
/// `None` (meaning: use the model's own default cadence).
#[cfg(feature = "python")]
pub fn parse_rebalance_arg(value: &pyo3::Bound<'_, pyo3::PyAny>) -> Option<TimeSpan> {
    use pyo3::types::{PyAnyMethods, PyTypeMethods};
    if let Ok(resolution) = value.extract::<crate::types::Resolution>() {
        return resolution_rebalance_period(resolution.into());
    }
    // `timedelta` must be tried before `f64`: pyo3 will not accidentally
    // extract a `timedelta` as a float, but checking it first keeps the
    // common `EqualWeightingPortfolioConstructionModel(timedelta(days=1))`
    // call from silently falling through to the default cadence.
    if let Ok(duration) = value.extract::<chrono::Duration>() {
        return duration
            .num_nanoseconds()
            .filter(|nanos| *nanos > 0)
            .map(TimeSpan::from_nanos);
    }
    if let Ok(seconds) = value.extract::<f64>() {
        return seconds_rebalance_period(seconds);
    }
    let type_name = value
        .get_type()
        .name()
        .map(|name| name.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());
    tracing::warn!(
        "unsupported rebalance argument type {type_name}; using model's default cadence"
    );
    None
}

#[cfg(feature = "python")]
fn parse_rebalance_policy_arg(value: &pyo3::Bound<'_, pyo3::PyAny>) -> Option<RebalancePolicy> {
    use pyo3::types::PyAnyMethods;

    if value.is_callable() {
        let callable = std::sync::Arc::new(value.clone().unbind());
        return Some(RebalancePolicy::next_time(move |now| {
            call_python_rebalance_func(
                callable.as_ref(),
                "EqualWeightingPortfolioConstructionModel",
                now,
            )
        }));
    }
    parse_rebalance_arg(value).map(RebalancePolicy::period)
}

#[cfg(feature = "python")]
pub(crate) fn call_python_rebalance_func(
    callable: &pyo3::Py<pyo3::PyAny>,
    name: &str,
    now: DateTime,
) -> Option<DateTime> {
    use pyo3::types::PyAnyMethods;

    pyo3::Python::attach(|py| -> pyo3::PyResult<Option<DateTime>> {
        let naive_utc = now.to_utc().naive_utc();
        let result = callable.bind(py).call1((naive_utc,))?;
        if result.is_none() {
            return Ok(None);
        }
        let next: chrono::NaiveDateTime = result.extract()?;
        Ok(Some(DateTime::from(next)))
    })
    .unwrap_or_else(|error| {
        tracing::warn!("Python PCM {name} rebalance function failed: {error}");
        None
    })
}

pub fn sanitize_positive_f64_decimal(value: Option<f64>) -> Option<Decimal> {
    value
        .filter(|value| value.is_finite() && *value > 0.0)
        .and_then(Decimal::from_f64_retain)
}

pub fn decimal_or_zero(value: f64) -> Decimal {
    Decimal::from_f64(value).unwrap_or(Decimal::ZERO)
}

pub fn decimal_or(value: f64, default: Decimal) -> Decimal {
    Decimal::from_f64(value).unwrap_or(default)
}

pub fn absolute_decimal_or_zero(value: f64) -> Decimal {
    Decimal::from_f64(value.abs()).unwrap_or(Decimal::ZERO)
}

pub fn insight_direction_from_str(direction: &str) -> AlphaInsightDirection {
    match direction.to_lowercase().as_str() {
        "up" => AlphaInsightDirection::Up,
        "down" => AlphaInsightDirection::Down,
        _ => AlphaInsightDirection::Flat,
    }
}

pub fn pcm_direction_from_alpha(direction: AlphaInsightDirection) -> PcmInsightDirection {
    match direction {
        AlphaInsightDirection::Up => PcmInsightDirection::Up,
        AlphaInsightDirection::Down => PcmInsightDirection::Down,
        AlphaInsightDirection::Flat => PcmInsightDirection::Flat,
    }
}

pub fn alpha_direction_from_pcm(direction: PcmInsightDirection) -> AlphaInsightDirection {
    match direction {
        PcmInsightDirection::Up => AlphaInsightDirection::Up,
        PcmInsightDirection::Down => AlphaInsightDirection::Down,
        PcmInsightDirection::Flat => AlphaInsightDirection::Flat,
    }
}

fn alpha_direction_from_sdk(direction: InsightDirection) -> AlphaInsightDirection {
    match direction {
        InsightDirection::Up => AlphaInsightDirection::Up,
        InsightDirection::Down => AlphaInsightDirection::Down,
        InsightDirection::Flat => AlphaInsightDirection::Flat,
    }
}

fn sdk_direction_from_alpha(direction: AlphaInsightDirection) -> InsightDirection {
    match direction {
        AlphaInsightDirection::Up => InsightDirection::Up,
        AlphaInsightDirection::Down => InsightDirection::Down,
        AlphaInsightDirection::Flat => InsightDirection::Flat,
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "InsightDirection", eq, eq_int)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsightDirection {
    Flat = 0,
    Up = 1,
    Down = 2,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "PortfolioBias", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortfolioBiasView {
    LongShort = 0,
    Long = 1,
    Short = 2,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "AlphaModel", subclass))]
pub struct AlphaModel;

impl AlphaModel {
    pub fn new() -> Self {
        Self
    }

    pub fn update(&self) -> Vec<InsightProjection> {
        Vec::new()
    }

    pub fn on_securities_changed(&self) {}
}

impl Default for AlphaModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "ExecutionModel", subclass))]
pub struct ExecutionModel;

impl ExecutionModel {
    pub fn new() -> Self {
        Self
    }

    pub fn execute(&self) {}
}

impl Default for ExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "PortfolioConstructionModel", subclass)
)]
pub struct PortfolioConstructionModel;

impl PortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }

    pub fn create_targets(&self) -> Vec<PortfolioTargetProjection> {
        Vec::new()
    }

    pub fn get_target_insights(&self) -> Vec<InsightProjection> {
        Vec::new()
    }
}

impl Default for PortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "RiskManagementModel", subclass)
)]
pub struct RiskManagementModel;

impl RiskManagementModel {
    pub fn new() -> Self {
        Self
    }

    pub fn manage_risk(&self) -> Vec<PortfolioTargetProjection> {
        Vec::new()
    }
}

impl Default for RiskManagementModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl AlphaModel {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn py_new(
        _args: &pyo3::Bound<'_, pyo3::types::PyTuple>,
        _kwargs: Option<&pyo3::Bound<'_, pyo3::types::PyDict>>,
    ) -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl ExecutionModel {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn py_new(
        _args: &pyo3::Bound<'_, pyo3::types::PyTuple>,
        _kwargs: Option<&pyo3::Bound<'_, pyo3::types::PyDict>>,
    ) -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl PortfolioConstructionModel {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn py_new(
        _args: &pyo3::Bound<'_, pyo3::types::PyTuple>,
        _kwargs: Option<&pyo3::Bound<'_, pyo3::types::PyDict>>,
    ) -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl RiskManagementModel {
    #[new]
    #[pyo3(signature = (*_args, **_kwargs))]
    fn py_new(
        _args: &pyo3::Bound<'_, pyo3::types::PyTuple>,
        _kwargs: Option<&pyo3::Bound<'_, pyo3::types::PyDict>>,
    ) -> Self {
        Self::new()
    }
}

pub fn create_constant_alpha_model(
    direction: &str,
    period_days: i64,
    magnitude: Option<f64>,
) -> Box<dyn IAlphaModel> {
    Box::new(rlean_alpha::ConstantAlphaModel {
        direction: insight_direction_from_str(direction),
        period: days_to_timespan(period_days),
        magnitude: magnitude.and_then(Decimal::from_f64),
    })
}

pub fn create_ema_cross_alpha_model(
    fast_period: usize,
    slow_period: usize,
    period_days: i64,
) -> Box<dyn IAlphaModel> {
    Box::new(rlean_alpha::EmaCrossAlphaModel::new(
        fast_period,
        slow_period,
        days_to_timespan(period_days),
    ))
}

pub fn create_historical_returns_alpha_model(
    period: usize,
    insight_period_days: Option<i64>,
) -> Box<dyn IAlphaModel> {
    Box::new(rlean_alpha::HistoricalReturnsAlphaModel::new(
        period,
        days_to_timespan(insight_period_days.unwrap_or(period as i64)),
    ))
}

pub fn create_pearson_pairs_alpha_model(
    lookback: usize,
    threshold: f64,
    minimum_correlation: f64,
    insight_period_days: Option<i64>,
) -> Box<dyn IAlphaModel> {
    Box::new(rlean_alpha::PearsonCorrelationPairsTradingAlphaModel::new(
        lookback,
        days_to_timespan(insight_period_days.unwrap_or(lookback as i64)),
        threshold,
        minimum_correlation,
    ))
}

pub fn create_macd_alpha_model(
    fast_period: usize,
    slow_period: usize,
    signal_period: usize,
    period_days: i64,
) -> Box<dyn IAlphaModel> {
    Box::new(rlean_alpha::MacdAlphaModel::new(
        fast_period,
        slow_period,
        signal_period,
        days_to_timespan(period_days),
    ))
}

pub fn create_rsi_alpha_model(period: usize, period_days: i64) -> Box<dyn IAlphaModel> {
    Box::new(rlean_alpha::RsiAlphaModel::new(
        period,
        days_to_timespan(period_days),
    ))
}

pub fn create_equal_weighting_pcm(
    portfolio_bias: PortfolioBias,
    max_weight: Option<f64>,
    rebalance_period: Option<TimeSpan>,
) -> Box<dyn IPortfolioConstructionModel> {
    create_equal_weighting_pcm_with_policy(
        portfolio_bias,
        max_weight,
        RebalancePolicy::from_period(rebalance_period),
    )
}

/// Construct equal weighting with a language-neutral rebalance policy.
pub fn create_equal_weighting_pcm_with_policy(
    portfolio_bias: PortfolioBias,
    max_weight: Option<f64>,
    rebalance_policy: RebalancePolicy,
) -> Box<dyn IPortfolioConstructionModel> {
    Box::new(
        LeanEqualWeightingPortfolioConstructionModel::with_bias_max_weight_and_rebalance_policy(
            portfolio_bias,
            sanitize_positive_f64_decimal(max_weight),
            rebalance_policy,
        ),
    )
}

pub fn create_insight_weighting_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(rlean_portfolio_construction::InsightWeightingPortfolioConstructionModel::new())
}

pub fn create_mean_variance_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(rlean_portfolio_construction::MeanVariancePortfolioConstructionModel::new())
}

pub fn create_max_sharpe_ratio_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(rlean_portfolio_construction::MaximumSharpeRatioPortfolioConstructionModel::new())
}

#[allow(clippy::too_many_arguments)]
pub fn create_black_litterman_pcm(
    rebalance_period: Option<TimeSpan>,
    portfolio_bias: PortfolioBias,
    lookback: usize,
    period: usize,
    risk_free_rate: f64,
    delta: f64,
    tau: f64,
    target_gross: f64,
) -> Box<dyn IPortfolioConstructionModel> {
    Box::new(
        BlackLittermanOptimizationPortfolioConstructionModel::with_params_and_rebalance(
            lookback,
            period,
            risk_free_rate,
            delta,
            tau,
            portfolio_bias,
            rebalance_period,
            target_gross,
        ),
    )
}

pub fn create_risk_parity_pcm(
    lookback: usize,
    period: usize,
) -> Box<dyn IPortfolioConstructionModel> {
    Box::new(RiskParityPortfolioConstructionModel::with_params(
        lookback, period,
    ))
}

pub fn create_confidence_weighting_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(ConfidenceWeightingPortfolioConstructionModel::new())
}

pub fn create_accumulative_insight_pcm(percent: f64) -> Box<dyn IPortfolioConstructionModel> {
    Box::new(AccumulativeInsightPortfolioConstructionModel::with_percent(
        decimal_or(percent, Decimal::new(3, 2)),
    ))
}

pub fn create_mean_reversion_pcm(
    reversion_threshold: f64,
    window_size: usize,
) -> Box<dyn IPortfolioConstructionModel> {
    Box::new(MeanReversionPortfolioConstructionModel::with_params(
        reversion_threshold,
        window_size,
    ))
}

pub fn create_immediate_execution_model() -> Box<dyn IExecutionModel> {
    Box::new(LeanImmediateExecutionModel::new())
}

pub fn create_null_execution_model() -> Box<dyn IExecutionModel> {
    Box::new(rlean_execution::NullExecutionModel::new())
}

pub fn create_vwap_execution_model(
    maximum_order_quantity_percent_volume: f64,
) -> (Box<dyn IExecutionModel>, f64) {
    let rate_f64 = maximum_order_quantity_percent_volume.abs();
    (
        Box::new(rlean_execution::VwapExecutionModel::new(decimal_or_zero(
            rate_f64,
        ))),
        rate_f64,
    )
}

pub fn create_spread_execution_model(accepting_spread_percent: f64) -> Box<dyn IExecutionModel> {
    Box::new(rlean_execution::SpreadExecutionModel::new(decimal_or_zero(
        accepting_spread_percent,
    )))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PassiveMakerExecutionSpec {
    pub max_passive_attempts: usize,
    pub adverse_selection_threshold: f64,
    pub maximum_order_value: f64,
}

pub fn passive_maker_execution_spec(
    max_passive_attempts: usize,
    adverse_selection_threshold: f64,
    maximum_order_value: f64,
) -> PassiveMakerExecutionSpec {
    PassiveMakerExecutionSpec {
        max_passive_attempts: max_passive_attempts.max(1),
        adverse_selection_threshold: adverse_selection_threshold.abs(),
        maximum_order_value: maximum_order_value.abs(),
    }
}

pub fn create_passive_maker_execution_model(
    spec: PassiveMakerExecutionSpec,
) -> Box<dyn IExecutionModel> {
    Box::new(
        rlean_execution::PassiveMakerExecutionModel::with_maximum_order_value(
            spec.max_passive_attempts,
            decimal_or_zero(spec.adverse_selection_threshold),
            decimal_or_zero(spec.maximum_order_value),
        ),
    )
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveMakerTakerExecutionSpec {
    pub accepting_spread_percent: f64,
    pub max_passive_attempts: usize,
    pub adverse_selection_threshold: f64,
    pub passive_duration_seconds: f64,
}

pub fn adaptive_maker_taker_execution_spec(
    accepting_spread_percent: f64,
    max_passive_attempts: usize,
    adverse_selection_threshold: f64,
    passive_duration_seconds: Option<f64>,
) -> AdaptiveMakerTakerExecutionSpec {
    let max_passive_attempts = max_passive_attempts.max(1);
    AdaptiveMakerTakerExecutionSpec {
        accepting_spread_percent: accepting_spread_percent.abs(),
        max_passive_attempts,
        adverse_selection_threshold: adverse_selection_threshold.abs(),
        passive_duration_seconds: passive_duration_seconds
            .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
            .unwrap_or(max_passive_attempts as f64 * 60.0),
    }
}

pub fn create_adaptive_maker_taker_execution_model(
    spec: AdaptiveMakerTakerExecutionSpec,
) -> Box<dyn IExecutionModel> {
    Box::new(AdaptiveMakerTakerExecutionModel::with_passive_duration(
        decimal_or_zero(spec.accepting_spread_percent),
        TimeSpan::from_millis((spec.passive_duration_seconds * 1000.0).round() as i64),
        decimal_or_zero(spec.adverse_selection_threshold),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MakerThenTakerExecutionSpec {
    pub passive_duration_seconds: f64,
    pub adverse_selection_threshold: f64,
    pub maximum_order_value: f64,
}

pub fn maker_then_taker_execution_spec(
    passive_duration_seconds: f64,
    adverse_selection_threshold: f64,
    maximum_order_value: f64,
) -> MakerThenTakerExecutionSpec {
    MakerThenTakerExecutionSpec {
        passive_duration_seconds: passive_duration_seconds.max(0.0),
        adverse_selection_threshold: adverse_selection_threshold.abs(),
        maximum_order_value: maximum_order_value.abs(),
    }
}

pub fn create_maker_then_taker_execution_model(
    spec: MakerThenTakerExecutionSpec,
) -> Box<dyn IExecutionModel> {
    Box::new(MakerThenTakerExecutionModel::with_maximum_order_value(
        TimeSpan::from_millis((spec.passive_duration_seconds * 1000.0).round() as i64),
        decimal_or_zero(spec.adverse_selection_threshold),
        decimal_or_zero(spec.maximum_order_value),
    ))
}

pub fn create_aggressive_post_only_execution_model(
    maximum_order_value: f64,
) -> (Box<dyn IExecutionModel>, f64) {
    let maximum_order_value = maximum_order_value.abs();
    (
        Box::new(AggressivePostOnlyExecutionModel::with_maximum_order_value(
            decimal_or_zero(maximum_order_value),
        )),
        maximum_order_value,
    )
}

pub fn standard_deviation_execution_period(period: i64) -> usize {
    usize::try_from(period).unwrap_or(60).max(1)
}

pub fn create_standard_deviation_execution_model(
    period: usize,
    deviations: f64,
    maximum_order_value: f64,
) -> Box<dyn IExecutionModel> {
    Box::new(
        rlean_execution::StandardDeviationExecutionModel::with_maximum_order_value(
            period,
            decimal_or(deviations, Decimal::from(2)),
            decimal_or_zero(maximum_order_value.abs()),
        ),
    )
}

pub fn create_null_risk_management_model() -> Box<dyn RiskManagementModelTrait> {
    Box::new(NullRiskManagement)
}

pub fn create_max_drawdown_percent_per_security(
    maximum_drawdown_percent: f64,
) -> Box<dyn RiskManagementModelTrait> {
    Box::new(rlean_risk::MaximumDrawdownPercentPerSecurity::new(
        decimal_or_zero(maximum_drawdown_percent),
    ))
}

pub fn create_trailing_stop_risk_model(trailing_amount: f64) -> Box<dyn RiskManagementModelTrait> {
    Box::new(rlean_risk::TrailingStopRiskManagementModel::new(
        decimal_or_zero(trailing_amount),
    ))
}

pub fn create_max_sector_exposure_risk_model(
    maximum_sector_exposure: f64,
) -> Box<dyn RiskManagementModelTrait> {
    Box::new(MaximumSectorExposureRiskManagementModel::new(
        decimal_or_zero(maximum_sector_exposure),
    ))
}

pub fn create_max_drawdown_percent_portfolio(
    maximum_drawdown_percent: f64,
    is_trailing: bool,
) -> Box<dyn RiskManagementModelTrait> {
    Box::new(MaximumDrawdownPercentPortfolio::new(
        decimal_or_zero(maximum_drawdown_percent),
        is_trailing,
    ))
}

pub fn create_max_unrealized_profit_per_security(
    maximum_unrealized_profit_percent: f64,
) -> Box<dyn RiskManagementModelTrait> {
    Box::new(MaximumUnrealizedProfitPercentPerSecurity::new(
        decimal_or_zero(maximum_unrealized_profit_percent),
    ))
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "InsightWeightingPortfolioConstructionModel")
)]
pub struct InsightWeightingPortfolioConstructionModel;

impl InsightWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for InsightWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "EqualWeightingPortfolioConstructionModel")
)]
#[derive(Clone)]
pub struct EqualWeightingPortfolioConstructionModel {
    pub portfolio_bias: PortfolioBias,
    pub max_weight: Option<f64>,
    pub rebalance_policy: RebalancePolicy,
    pub rebalance_on_security_changes: bool,
    pub rebalance_on_insight_changes: bool,
}

impl EqualWeightingPortfolioConstructionModel {
    pub fn new() -> Self {
        Self {
            portfolio_bias: PortfolioBias::LongShort,
            max_weight: None,
            rebalance_policy: default_rebalance_policy(),
            rebalance_on_security_changes: true,
            rebalance_on_insight_changes: true,
        }
    }

    pub fn into_pcm(self) -> Box<dyn IPortfolioConstructionModel> {
        create_equal_weighting_pcm_with_policy(
            self.portfolio_bias,
            self.max_weight,
            self.rebalance_policy
                .with_security_changes(self.rebalance_on_security_changes)
                .with_insight_changes(self.rebalance_on_insight_changes),
        )
    }
}

impl Default for EqualWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "MeanVarianceOptimizationPortfolioConstructionModel")
)]
pub struct MeanVarianceOptimizationPortfolioConstructionModel;

impl MeanVarianceOptimizationPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MeanVarianceOptimizationPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "MaximumSharpeRatioPortfolioConstructionModel")
)]
pub struct MaximumSharpeRatioPortfolioConstructionModel;

impl MaximumSharpeRatioPortfolioConstructionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MaximumSharpeRatioPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "ImmediateExecutionModel"))]
pub struct ImmediateExecutionModel;

impl ImmediateExecutionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for ImmediateExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "NullExecutionModel"))]
pub struct NullExecutionModel;

impl NullExecutionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NullExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "VWAPExecutionModel"))]
pub struct VwapExecutionModel;

impl VwapExecutionModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VwapExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "StandardDeviationExecutionModel")
)]
pub struct StandardDeviationExecutionModel;

impl StandardDeviationExecutionModel {
    pub fn new(_period: usize, _deviations: f64) -> Self {
        Self
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "NullRiskManagementModel"))]
pub struct NullRiskManagementModel;

impl NullRiskManagementModel {
    pub fn new() -> Self {
        Self
    }
}

impl Default for NullRiskManagementModel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "MaximumDrawdownPercentPerSecurity")
)]
pub struct MaximumDrawdownPercentPerSecurity {
    pub maximum_drawdown_percent: f64,
}

impl MaximumDrawdownPercentPerSecurity {
    pub fn new(maximum_drawdown_percent: f64) -> Self {
        Self {
            maximum_drawdown_percent,
        }
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "TrailingStopRiskManagementModel")
)]
pub struct TrailingStopRiskManagementModel {
    pub trailing_amount: f64,
}

impl TrailingStopRiskManagementModel {
    pub fn new(trailing_amount: f64) -> Self {
        Self { trailing_amount }
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "MaximumUnrealizedProfitPercentPerSecurity")
)]
pub struct MaximumUnrealizedProfitPercentPerSecurityModel {
    pub maximum_unrealized_profit_percent: f64,
}

impl MaximumUnrealizedProfitPercentPerSecurityModel {
    pub fn new(maximum_unrealized_profit_percent: f64) -> Self {
        Self {
            maximum_unrealized_profit_percent,
        }
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "ConstantAlphaModel"))]
pub struct ConstantAlphaModel;

impl ConstantAlphaModel {
    pub fn new(_direction: String, _period_days: i64, _magnitude: f64) -> Self {
        Self
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "EmaCrossAlphaModel"))]
pub struct EmaCrossAlphaModel;

impl EmaCrossAlphaModel {
    pub fn new(_fast_period: usize, _slow_period: usize, _period_days: i64) -> Self {
        Self
    }
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "HistoricalReturnsAlphaModel")
)]
pub struct HistoricalReturnsAlphaModel;

impl HistoricalReturnsAlphaModel {
    pub fn new(_period: usize, _insight_period_days: i64) -> Self {
        Self
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "MacdAlphaModel"))]
pub struct MacdAlphaModel;

impl MacdAlphaModel {
    pub fn new(
        _fast_period: usize,
        _slow_period: usize,
        _signal_period: usize,
        _period_days: i64,
    ) -> Self {
        Self
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "RsiAlphaModel"))]
pub struct RsiAlphaModel;

impl RsiAlphaModel {
    pub fn new(_period: usize, _period_days: i64) -> Self {
        Self
    }
}

#[cfg(feature = "python")]
macro_rules! py_model_new_no_args {
    ($ty:ty) => {
        #[pyo3::pymethods]
        impl $ty {
            #[new]
            fn py_new() -> Self {
                Self::new()
            }
        }
    };
}

#[cfg(feature = "python")]
py_model_new_no_args!(InsightWeightingPortfolioConstructionModel);
#[cfg(feature = "python")]
#[pyo3::pymethods]
impl EqualWeightingPortfolioConstructionModel {
    #[new]
    #[pyo3(signature = (rebalance=None, portfolio_bias=None, max_weight=None))]
    fn py_new(
        rebalance: Option<pyo3::Bound<'_, pyo3::PyAny>>,
        portfolio_bias: Option<PortfolioBiasView>,
        max_weight: Option<f64>,
    ) -> Self {
        let rebalance_policy = rebalance
            .as_ref()
            .and_then(parse_rebalance_policy_arg)
            .unwrap_or_else(default_rebalance_policy);
        let portfolio_bias = portfolio_bias
            .map(portfolio_bias_from_view)
            .unwrap_or(PortfolioBias::LongShort);
        Self {
            portfolio_bias,
            max_weight,
            rebalance_policy,
            rebalance_on_security_changes: true,
            rebalance_on_insight_changes: true,
        }
    }

    #[getter]
    fn rebalance_on_security_changes(&self) -> bool {
        self.rebalance_on_security_changes
    }

    #[setter]
    fn set_rebalance_on_security_changes(&mut self, enabled: bool) {
        self.rebalance_on_security_changes = enabled;
    }

    #[getter]
    fn rebalance_on_insight_changes(&self) -> bool {
        self.rebalance_on_insight_changes
    }

    #[setter]
    fn set_rebalance_on_insight_changes(&mut self, enabled: bool) {
        self.rebalance_on_insight_changes = enabled;
    }
}
#[cfg(feature = "python")]
py_model_new_no_args!(MeanVarianceOptimizationPortfolioConstructionModel);
#[cfg(feature = "python")]
py_model_new_no_args!(MaximumSharpeRatioPortfolioConstructionModel);
#[cfg(feature = "python")]
py_model_new_no_args!(ImmediateExecutionModel);
#[cfg(feature = "python")]
py_model_new_no_args!(NullExecutionModel);
#[cfg(feature = "python")]
py_model_new_no_args!(VwapExecutionModel);
#[cfg(feature = "python")]
py_model_new_no_args!(NullRiskManagementModel);

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl StandardDeviationExecutionModel {
    #[new]
    fn py_new(period: usize, deviations: f64) -> Self {
        Self::new(period, deviations)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl MaximumDrawdownPercentPerSecurity {
    #[new]
    fn py_new(maximum_drawdown_percent: f64) -> Self {
        Self::new(maximum_drawdown_percent)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl TrailingStopRiskManagementModel {
    #[new]
    fn py_new(trailing_amount: f64) -> Self {
        Self::new(trailing_amount)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl MaximumUnrealizedProfitPercentPerSecurityModel {
    #[new]
    fn py_new(maximum_unrealized_profit_percent: f64) -> Self {
        Self::new(maximum_unrealized_profit_percent)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl ConstantAlphaModel {
    #[new]
    fn py_new(direction: String, period_days: i64, magnitude: f64) -> Self {
        Self::new(direction, period_days, magnitude)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl EmaCrossAlphaModel {
    #[new]
    fn py_new(fast_period: usize, slow_period: usize, period_days: i64) -> Self {
        Self::new(fast_period, slow_period, period_days)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl HistoricalReturnsAlphaModel {
    #[new]
    fn py_new(period: usize, insight_period_days: i64) -> Self {
        Self::new(period, insight_period_days)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl MacdAlphaModel {
    #[new]
    fn py_new(
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        period_days: i64,
    ) -> Self {
        Self::new(fast_period, slow_period, signal_period, period_days)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl RsiAlphaModel {
    #[new]
    fn py_new(period: usize, period_days: i64) -> Self {
        Self::new(period, period_days)
    }
}

/// Insight validity period — accepts `int` days, `float` days, or `datetime.timedelta` from Python.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsightPeriod(pub TimeSpan);

impl InsightPeriod {
    pub fn from_days(days: i64) -> Self {
        Self(TimeSpan::from_days(days))
    }

    pub fn as_timespan(self) -> TimeSpan {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "Insight"))]
pub struct InsightProjection {
    pub id: u64,
    pub symbol: Symbol,
    pub direction: AlphaInsightDirection,
    pub period_nanos: i64,
    pub magnitude: Option<f64>,
    pub confidence: Option<f64>,
    pub source_model: String,
    pub score_direction: Option<f64>,
    pub score_magnitude: Option<f64>,
    pub is_final_score: bool,
    pub generated_time_utc: DateTime,
    pub close_time_utc: DateTime,
}

impl InsightProjection {
    pub fn price(
        symbol: Symbol,
        period: InsightPeriod,
        direction: InsightDirection,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
        weight: Option<f64>,
    ) -> Self {
        let _ = weight;
        project_alpha_insight(&Insight::new(
            symbol,
            alpha_direction_from_sdk(direction),
            period.as_timespan(),
            magnitude.and_then(Decimal::from_f64),
            confidence.and_then(Decimal::from_f64),
            source_model.as_deref().unwrap_or_default(),
        ))
    }
    pub fn price_value(&self) -> Option<f64> {
        self.magnitude
    }

    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    pub fn direction(&self) -> InsightDirection {
        sdk_direction_from_alpha(self.direction)
    }
    pub fn magnitude(&self) -> Option<f64> {
        self.magnitude
    }
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }

    pub fn source_model(&self) -> String {
        self.source_model.clone()
    }
    pub fn score_direction_value(&self) -> Option<f64> {
        self.score_direction
    }

    /// Monotonic insight identifier. Mirrors LEAN `Insight.Id`.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Insight prediction period. Mirrors LEAN `Insight.Period`.
    pub fn period(&self) -> chrono::Duration {
        chrono::Duration::nanoseconds(self.period_nanos)
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl InsightProjection {
    #[staticmethod]
    #[pyo3(name = "price", signature = (symbol, period, direction, magnitude=None, confidence=None, source_model=None, weight=None))]
    fn py_price(
        symbol: crate::securities::SymbolHandle,
        period: &pyo3::Bound<'_, pyo3::PyAny>,
        direction: InsightDirection,
        magnitude: Option<f64>,
        confidence: Option<f64>,
        source_model: Option<String>,
        weight: Option<f64>,
    ) -> pyo3::PyResult<Self> {
        let period = crate::python_framework::insight_period_from_py(period)?;
        let _ = weight;
        Ok(project_alpha_insight(&Insight::new(
            symbol.into_inner(),
            alpha_direction_from_sdk(direction),
            period,
            magnitude.and_then(Decimal::from_f64),
            confidence.and_then(Decimal::from_f64),
            source_model.as_deref().unwrap_or_default(),
        )))
    }

    #[staticmethod]
    #[pyo3(name = "Price", signature = (symbol, period, direction))]
    fn py_price_pascal(
        symbol: crate::securities::SymbolHandle,
        period: &pyo3::Bound<'_, pyo3::PyAny>,
        direction: InsightDirection,
    ) -> pyo3::PyResult<Self> {
        Self::py_price(symbol, period, direction, None, None, None, None)
    }

    #[getter(id)]
    fn py_id(&self) -> u64 {
        self.id()
    }

    #[getter(period)]
    fn py_period(&self) -> chrono::Duration {
        self.period()
    }

    #[getter(symbol)]
    fn py_symbol(&self) -> crate::securities::SymbolHandle {
        crate::securities::SymbolHandle::new(self.symbol.clone())
    }

    #[getter(direction)]
    fn py_direction(&self) -> InsightDirection {
        self.direction()
    }

    #[getter(magnitude)]
    fn py_magnitude(&self) -> Option<f64> {
        self.magnitude()
    }

    #[getter(source_model)]
    fn py_source_model(&self) -> String {
        self.source_model()
    }

    #[getter(confidence)]
    fn py_confidence(&self) -> Option<f64> {
        self.confidence()
    }

    #[getter(score_direction)]
    fn py_score_direction(&self) -> Option<f64> {
        self.score_direction
    }

    #[getter(score_magnitude)]
    fn py_score_magnitude(&self) -> Option<f64> {
        self.score_magnitude
    }

    #[getter(is_final_score)]
    fn py_is_final_score(&self) -> bool {
        self.is_final_score
    }

    #[getter(generated_time_utc)]
    fn py_generated_time_utc(&self) -> chrono::NaiveDateTime {
        self.generated_time_utc.to_utc().naive_utc()
    }

    #[getter(close_time_utc)]
    fn py_close_time_utc(&self) -> chrono::NaiveDateTime {
        self.close_time_utc.to_utc().naive_utc()
    }
}

pub fn project_alpha_insight(insight: &Insight) -> InsightProjection {
    InsightProjection {
        id: insight.id,
        symbol: insight.symbol.clone(),
        direction: insight.direction,
        period_nanos: insight.period.nanos,
        magnitude: insight.magnitude.and_then(|m| m.to_f64()),
        confidence: insight.confidence.and_then(|c| c.to_f64()),
        source_model: insight.source_model.to_string(),
        score_direction: insight.score_direction,
        score_magnitude: insight.score_magnitude,
        is_final_score: insight.is_final_score,
        generated_time_utc: insight.generated_time_utc,
        close_time_utc: insight.close_time_utc,
    }
}

pub fn project_pcm_insight(insight: &InsightForPcm, utc_now: DateTime) -> InsightProjection {
    InsightProjection {
        id: 0,
        symbol: insight.symbol.clone(),
        direction: alpha_direction_from_pcm(insight.direction),
        period_nanos: TimeSpan::ONE_DAY.nanos,
        magnitude: insight.magnitude.and_then(|m| m.to_f64()),
        confidence: insight.confidence.and_then(|c| c.to_f64()),
        source_model: insight.source_model.clone(),
        score_direction: None,
        score_magnitude: None,
        is_final_score: false,
        generated_time_utc: utc_now,
        close_time_utc: utc_now + TimeSpan::ONE_DAY,
    }
}

pub fn insight_from_projection(projection: &InsightProjection) -> Insight {
    Insight::new(
        projection.symbol.clone(),
        projection.direction,
        TimeSpan::from_nanos(projection.period_nanos),
        projection.magnitude.and_then(Decimal::from_f64),
        projection.confidence.and_then(Decimal::from_f64),
        &projection.source_model,
    )
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "PortfolioTarget"))]
pub struct PortfolioTargetProjection {
    pub symbol: Symbol,
    pub quantity: Option<f64>,
    pub percent: Option<f64>,
    pub tag: String,
}

impl PortfolioTargetProjection {
    pub fn new(
        symbol: Symbol,
        quantity: Option<f64>,
        percent: Option<f64>,
        tag: Option<String>,
    ) -> Self {
        Self {
            symbol,
            quantity,
            percent,
            tag: tag.unwrap_or_default(),
        }
    }

    pub fn percent(symbol: Symbol, percent: f64) -> Self {
        Self {
            symbol,
            quantity: None,
            percent: Some(percent),
            tag: String::new(),
        }
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl PortfolioTargetProjection {
    #[staticmethod]
    #[pyo3(name = "percent", signature = (symbol, percent, tag=None))]
    fn py_percent(
        symbol: crate::securities::SymbolHandle,
        percent: f64,
        tag: Option<String>,
    ) -> Self {
        Self {
            symbol: symbol.into_inner(),
            quantity: None,
            percent: Some(percent),
            tag: tag.unwrap_or_default(),
        }
    }

    #[staticmethod]
    #[pyo3(name = "Percent", signature = (symbol, percent, tag=None))]
    fn py_percent_pascal(
        symbol: crate::securities::SymbolHandle,
        percent: f64,
        tag: Option<String>,
    ) -> Self {
        Self::py_percent(symbol, percent, tag)
    }

    #[new]
    #[pyo3(signature = (symbol, quantity, tag=None))]
    fn py_new(symbol: crate::securities::SymbolHandle, quantity: f64, tag: Option<String>) -> Self {
        Self {
            symbol: symbol.into_inner(),
            quantity: Some(quantity),
            percent: None,
            tag: tag.unwrap_or_default(),
        }
    }

    #[getter(symbol)]
    fn py_symbol(&self) -> crate::securities::SymbolHandle {
        crate::securities::SymbolHandle::new(self.symbol.clone())
    }

    #[getter(quantity)]
    fn py_quantity(&self) -> Option<f64> {
        self.quantity
    }

    #[getter(percent)]
    fn py_percent_getter(&self) -> Option<f64> {
        self.percent
    }

    #[getter(tag)]
    fn py_tag(&self) -> String {
        self.tag.clone()
    }
}

pub fn portfolio_target_from_projection(
    projection: &PortfolioTargetProjection,
    portfolio_value: Decimal,
    prices: &HashMap<u64, Decimal>,
) -> Option<PortfolioTarget> {
    if let Some(percent) = projection.percent {
        let percent = Decimal::from_f64(percent)?;
        let price = prices
            .get(&projection.symbol.id.sid)
            .copied()
            .unwrap_or(Decimal::ONE);
        return Some(PortfolioTarget::percent_with_tag(
            projection.symbol.clone(),
            percent,
            portfolio_value,
            price,
            projection.tag.clone(),
        ));
    }

    projection.quantity.and_then(|quantity| {
        Some(PortfolioTarget::new_with_tag(
            projection.symbol.clone(),
            Decimal::from_f64(quantity)?,
            projection.tag.clone(),
        ))
    })
}

pub fn portfolio_target_projection_from_execution_target(
    target: &ExecutionTarget,
) -> PortfolioTargetProjection {
    PortfolioTargetProjection {
        symbol: target.symbol.clone(),
        quantity: target.quantity.to_f64(),
        percent: None,
        tag: target.tag.clone(),
    }
}

pub fn portfolio_target_projection_from_risk_target(
    target: &RiskPortfolioTarget,
) -> PortfolioTargetProjection {
    PortfolioTargetProjection {
        symbol: target.symbol.clone(),
        quantity: target.quantity.to_f64(),
        percent: None,
        tag: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insight_direction_from_python_string_matches_lean_defaults() {
        assert_eq!(insight_direction_from_str("up"), AlphaInsightDirection::Up);
        assert_eq!(
            insight_direction_from_str("DOWN"),
            AlphaInsightDirection::Down
        );
        assert_eq!(
            insight_direction_from_str("sideways"),
            AlphaInsightDirection::Flat
        );
    }

    #[test]
    fn execution_specs_sanitize_python_constructor_values() {
        assert_eq!(
            passive_maker_execution_spec(0, -0.25, -10.0),
            PassiveMakerExecutionSpec {
                max_passive_attempts: 1,
                adverse_selection_threshold: 0.25,
                maximum_order_value: 10.0,
            }
        );

        assert_eq!(
            adaptive_maker_taker_execution_spec(-0.1, 0, -0.2, None),
            AdaptiveMakerTakerExecutionSpec {
                accepting_spread_percent: 0.1,
                max_passive_attempts: 1,
                adverse_selection_threshold: 0.2,
                passive_duration_seconds: 60.0,
            }
        );
    }

    #[test]
    fn rebalance_and_decimal_helpers_sanitize_sdk_inputs() {
        assert_eq!(
            resolution_rebalance_period(Resolution::Daily),
            Some(TimeSpan::ONE_DAY)
        );
        assert_eq!(
            seconds_rebalance_period(30.0),
            Some(TimeSpan::from_nanos(30_000_000_000))
        );
        assert_eq!(seconds_rebalance_period(0.0), None);
        assert_eq!(seconds_rebalance_period(f64::NAN), None);
        assert_eq!(
            sanitize_positive_f64_decimal(Some(2.5)),
            Some(Decimal::from_f64_retain(2.5).unwrap())
        );
        assert_eq!(sanitize_positive_f64_decimal(Some(-2.5)), None);
        assert_eq!(decimal_or(f64::NAN, Decimal::from(7)), Decimal::from(7));
        assert_eq!(
            absolute_decimal_or_zero(-3.5),
            Decimal::from_f64_retain(3.5).unwrap()
        );
    }

    #[cfg(feature = "python")]
    #[test]
    fn parse_rebalance_arg_accepts_timedelta_and_warns_on_unsupported_types() {
        use pyo3::types::{IntoPyDict, PyAnyMethods};
        use pyo3::Python;

        Python::attach(|py| {
            let timedelta_type = py.import("datetime").unwrap().getattr("timedelta").unwrap();
            let one_day = timedelta_type.call1((1,)).unwrap();
            assert_eq!(parse_rebalance_arg(&one_day), Some(TimeSpan::ONE_DAY));

            let kwargs = [("hours", 12)].into_py_dict(py).unwrap();
            let half_day = timedelta_type.call((), Some(&kwargs)).unwrap();
            assert_eq!(
                parse_rebalance_arg(&half_day),
                Some(TimeSpan::from_nanos(12 * 60 * 60 * 1_000_000_000))
            );

            // A type LEAN never accepts here (e.g. a list) must not silently
            // resolve to the model's default cadence via a bogus extraction —
            // it should fall through to `None` so the caller applies its own
            // documented default.
            let unsupported = pyo3::types::PyList::empty(py);
            assert_eq!(parse_rebalance_arg(unsupported.as_any()), None);
        });
    }

    #[cfg(feature = "python")]
    #[test]
    fn equal_weighting_python_callback_supports_insight_only_policy() {
        use pyo3::ffi::c_str;
        use pyo3::Python;
        use rlean_portfolio_construction::RebalanceCadence;

        Python::attach(|py| {
            let callback = py.eval(c_str!("lambda _: None"), None, None).unwrap();
            let mut model = EqualWeightingPortfolioConstructionModel::py_new(
                Some(callback),
                Some(PortfolioBiasView::Long),
                Some(0.2),
            );
            model.set_rebalance_on_security_changes(false);

            assert!(!model.rebalance_on_security_changes());
            assert!(model.rebalance_on_insight_changes());
            match model.rebalance_policy.cadence() {
                RebalanceCadence::NextTime(next) => {
                    assert_eq!(next(DateTime::now()), None);
                }
                _ => panic!("Python callback must produce a next-time policy"),
            }
        });
    }

    #[test]
    fn portfolio_target_projection_converts_percent_with_prices() {
        let symbol = Symbol::create_equity("SPY", &rlean_core::Market::usa());
        let projection = PortfolioTargetProjection {
            symbol: symbol.clone(),
            quantity: None,
            percent: Some(0.5),
            tag: "half".to_string(),
        };
        let prices = HashMap::from([(symbol.id.sid, Decimal::from(100))]);

        let target =
            portfolio_target_from_projection(&projection, Decimal::from(10_000), &prices).unwrap();

        assert_eq!(target.symbol, symbol);
        assert_eq!(target.quantity, Decimal::from(50));
        assert_eq!(target.tag, "half");
    }
}
