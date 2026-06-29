//! SDK-owned framework model and pipeline APIs.

use std::collections::HashMap;

use lean_alpha::{IAlphaModel, Insight, InsightDirection as AlphaInsightDirection};
use lean_core::{DateTime, Resolution, Symbol, TimeSpan};
use lean_execution::{
    AdaptiveMakerTakerExecutionModel, AggressivePostOnlyExecutionModel, IExecutionModel,
    ImmediateExecutionModel as LeanImmediateExecutionModel, MakerThenTakerExecutionModel,
};
use lean_portfolio_construction::{
    AccumulativeInsightPortfolioConstructionModel,
    BlackLittermanOptimizationPortfolioConstructionModel,
    ConfidenceWeightingPortfolioConstructionModel,
    EqualWeightingPortfolioConstructionModel as LeanEqualWeightingPortfolioConstructionModel,
    IPortfolioConstructionModel, InsightDirection as PcmInsightDirection, InsightForPcm,
    MeanReversionPortfolioConstructionModel, PortfolioBias, PortfolioTarget,
    RiskParityPortfolioConstructionModel,
};
use lean_risk::risk_management::{
    NullRiskManagement, RiskManagementModel as RiskManagementModelTrait,
};
use lean_risk::{
    MaximumDrawdownPercentPortfolio, MaximumSectorExposureRiskManagementModel,
    MaximumUnrealizedProfitPercentPerSecurity,
};
use lean_sdk_annotations::{sdk_bind, sdk_getter, sdk_method, sdk_new, sdk_static};
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

#[sdk_bind(
    py_name = "InsightDirection",
    rust_type = "lean_sdk::framework::InsightDirection"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsightDirection {
    Flat = 0,
    Up = 1,
    Down = 2,
}

#[sdk_bind(py_name = "PortfolioBias")]
pub enum PortfolioBiasView {
    LongShort = 0,
    Long = 1,
    Short = 2,
}

#[sdk_bind(py_name = "AlphaModel", subclass, constructor = "variadic")]
pub struct AlphaModel;

impl AlphaModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self
    }

    #[sdk_method]
    pub fn update(&self) -> Vec<InsightProjection> {
        Vec::new()
    }

    #[sdk_method]
    pub fn on_securities_changed(&self) {}
}

impl Default for AlphaModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "ExecutionModel", subclass, constructor = "variadic")]
pub struct ExecutionModel;

impl ExecutionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self
    }

    #[sdk_method]
    pub fn execute(&self) {}
}

impl Default for ExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(
    py_name = "PortfolioConstructionModel",
    subclass,
    constructor = "variadic"
)]
pub struct PortfolioConstructionModel;

impl PortfolioConstructionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self
    }

    #[sdk_method]
    pub fn create_targets(&self) -> Vec<PortfolioTargetProjection> {
        Vec::new()
    }

    #[sdk_method]
    pub fn get_target_insights(&self) -> Vec<InsightProjection> {
        Vec::new()
    }
}

impl Default for PortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "RiskManagementModel", subclass, constructor = "variadic")]
pub struct RiskManagementModel;

impl RiskManagementModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self
    }

    #[sdk_method]
    pub fn manage_risk(&self) -> Vec<PortfolioTargetProjection> {
        Vec::new()
    }
}

impl Default for RiskManagementModel {
    fn default() -> Self {
        Self::new()
    }
}

pub fn create_constant_alpha_model(
    direction: &str,
    period_days: i64,
    magnitude: Option<f64>,
) -> Box<dyn IAlphaModel> {
    Box::new(lean_alpha::ConstantAlphaModel {
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
    Box::new(lean_alpha::EmaCrossAlphaModel::new(
        fast_period,
        slow_period,
        days_to_timespan(period_days),
    ))
}

pub fn create_historical_returns_alpha_model(
    period: usize,
    insight_period_days: Option<i64>,
) -> Box<dyn IAlphaModel> {
    Box::new(lean_alpha::HistoricalReturnsAlphaModel::new(
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
    Box::new(lean_alpha::PearsonCorrelationPairsTradingAlphaModel::new(
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
    Box::new(lean_alpha::MacdAlphaModel::new(
        fast_period,
        slow_period,
        signal_period,
        days_to_timespan(period_days),
    ))
}

pub fn create_rsi_alpha_model(period: usize, period_days: i64) -> Box<dyn IAlphaModel> {
    Box::new(lean_alpha::RsiAlphaModel::new(
        period,
        days_to_timespan(period_days),
    ))
}

pub fn create_equal_weighting_pcm(
    portfolio_bias: PortfolioBias,
    max_weight: Option<f64>,
    rebalance_period: Option<TimeSpan>,
) -> Box<dyn IPortfolioConstructionModel> {
    Box::new(
        LeanEqualWeightingPortfolioConstructionModel::with_bias_max_weight_and_rebalance(
            portfolio_bias,
            sanitize_positive_f64_decimal(max_weight),
            rebalance_period,
        ),
    )
}

pub fn create_insight_weighting_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(lean_portfolio_construction::InsightWeightingPortfolioConstructionModel::new())
}

pub fn create_mean_variance_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(lean_portfolio_construction::MeanVariancePortfolioConstructionModel::new())
}

pub fn create_max_sharpe_ratio_pcm() -> Box<dyn IPortfolioConstructionModel> {
    Box::new(lean_portfolio_construction::MaximumSharpeRatioPortfolioConstructionModel::new())
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
    Box::new(lean_execution::NullExecutionModel::new())
}

pub fn create_vwap_execution_model(
    maximum_order_quantity_percent_volume: f64,
) -> (Box<dyn IExecutionModel>, f64) {
    let rate_f64 = maximum_order_quantity_percent_volume.abs();
    (
        Box::new(lean_execution::VwapExecutionModel::new(decimal_or_zero(
            rate_f64,
        ))),
        rate_f64,
    )
}

pub fn create_spread_execution_model(accepting_spread_percent: f64) -> Box<dyn IExecutionModel> {
    Box::new(lean_execution::SpreadExecutionModel::new(decimal_or_zero(
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
        lean_execution::PassiveMakerExecutionModel::with_maximum_order_value(
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
        lean_execution::StandardDeviationExecutionModel::with_maximum_order_value(
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
    Box::new(lean_risk::MaximumDrawdownPercentPerSecurity::new(
        decimal_or_zero(maximum_drawdown_percent),
    ))
}

pub fn create_trailing_stop_risk_model(trailing_amount: f64) -> Box<dyn RiskManagementModelTrait> {
    Box::new(lean_risk::TrailingStopRiskManagementModel::new(
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

#[sdk_bind(py_name = "InsightWeightingPortfolioConstructionModel")]
pub struct InsightWeightingPortfolioConstructionModel {
    #[allow(dead_code)]
    inner: Box<dyn IPortfolioConstructionModel>,
}

impl InsightWeightingPortfolioConstructionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_insight_weighting_pcm(),
        }
    }
}

impl Default for InsightWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "EqualWeightingPortfolioConstructionModel")]
pub struct EqualWeightingPortfolioConstructionModel {
    #[allow(dead_code)]
    inner: Box<dyn IPortfolioConstructionModel>,
}

impl EqualWeightingPortfolioConstructionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_equal_weighting_pcm(PortfolioBias::LongShort, None, None),
        }
    }
}

impl Default for EqualWeightingPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "MeanVarianceOptimizationPortfolioConstructionModel")]
pub struct MeanVarianceOptimizationPortfolioConstructionModel {
    #[allow(dead_code)]
    inner: Box<dyn IPortfolioConstructionModel>,
}

impl MeanVarianceOptimizationPortfolioConstructionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_mean_variance_pcm(),
        }
    }
}

impl Default for MeanVarianceOptimizationPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "MaximumSharpeRatioPortfolioConstructionModel")]
pub struct MaximumSharpeRatioPortfolioConstructionModel {
    #[allow(dead_code)]
    inner: Box<dyn IPortfolioConstructionModel>,
}

impl MaximumSharpeRatioPortfolioConstructionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_max_sharpe_ratio_pcm(),
        }
    }
}

impl Default for MaximumSharpeRatioPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "ImmediateExecutionModel")]
pub struct ImmediateExecutionModel {
    #[allow(dead_code)]
    inner: Box<dyn IExecutionModel>,
}

impl ImmediateExecutionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_immediate_execution_model(),
        }
    }
}

impl Default for ImmediateExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "NullExecutionModel")]
pub struct NullExecutionModel {
    #[allow(dead_code)]
    inner: Box<dyn IExecutionModel>,
}

impl NullExecutionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_null_execution_model(),
        }
    }
}

impl Default for NullExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "VWAPExecutionModel")]
pub struct VwapExecutionModel {
    #[allow(dead_code)]
    inner: (Box<dyn IExecutionModel>, f64),
}

impl VwapExecutionModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_vwap_execution_model(0.1),
        }
    }
}

impl Default for VwapExecutionModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "StandardDeviationExecutionModel")]
pub struct StandardDeviationExecutionModel {
    #[allow(dead_code)]
    inner: Box<dyn IExecutionModel>,
}

impl StandardDeviationExecutionModel {
    #[sdk_new]
    pub fn new(period: usize, deviations: f64) -> Self {
        Self {
            inner: create_standard_deviation_execution_model(period, deviations, 0.0),
        }
    }
}

#[sdk_bind(py_name = "NullRiskManagementModel")]
pub struct NullRiskManagementModel {
    #[allow(dead_code)]
    inner: Box<dyn RiskManagementModelTrait>,
}

impl NullRiskManagementModel {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: create_null_risk_management_model(),
        }
    }
}

impl Default for NullRiskManagementModel {
    fn default() -> Self {
        Self::new()
    }
}

#[sdk_bind(py_name = "MaximumDrawdownPercentPerSecurity")]
pub struct MaximumDrawdownPercentPerSecurity {
    #[allow(dead_code)]
    inner: Box<dyn RiskManagementModelTrait>,
}

impl MaximumDrawdownPercentPerSecurity {
    #[sdk_new]
    pub fn new(maximum_drawdown_percent: f64) -> Self {
        Self {
            inner: create_max_drawdown_percent_per_security(maximum_drawdown_percent),
        }
    }
}

#[sdk_bind(py_name = "TrailingStopRiskManagementModel")]
pub struct TrailingStopRiskManagementModel {
    #[allow(dead_code)]
    inner: Box<dyn RiskManagementModelTrait>,
}

impl TrailingStopRiskManagementModel {
    #[sdk_new]
    pub fn new(trailing_amount: f64) -> Self {
        Self {
            inner: create_trailing_stop_risk_model(trailing_amount),
        }
    }
}

#[sdk_bind(py_name = "ConstantAlphaModel")]
pub struct ConstantAlphaModel {
    #[allow(dead_code)]
    inner: Box<dyn IAlphaModel>,
}

impl ConstantAlphaModel {
    #[sdk_new]
    pub fn new(direction: String, period_days: i64, magnitude: f64) -> Self {
        Self {
            inner: create_constant_alpha_model(&direction, period_days, Some(magnitude)),
        }
    }
}

#[sdk_bind(py_name = "EmaCrossAlphaModel")]
pub struct EmaCrossAlphaModel {
    #[allow(dead_code)]
    inner: Box<dyn IAlphaModel>,
}

impl EmaCrossAlphaModel {
    #[sdk_new]
    pub fn new(fast_period: usize, slow_period: usize, period_days: i64) -> Self {
        Self {
            inner: create_ema_cross_alpha_model(fast_period, slow_period, period_days),
        }
    }
}

#[sdk_bind(py_name = "HistoricalReturnsAlphaModel")]
pub struct HistoricalReturnsAlphaModel {
    #[allow(dead_code)]
    inner: Box<dyn IAlphaModel>,
}

impl HistoricalReturnsAlphaModel {
    #[sdk_new]
    pub fn new(period: usize, insight_period_days: i64) -> Self {
        Self {
            inner: create_historical_returns_alpha_model(period, Some(insight_period_days)),
        }
    }
}

#[sdk_bind(py_name = "MacdAlphaModel")]
pub struct MacdAlphaModel {
    #[allow(dead_code)]
    inner: Box<dyn IAlphaModel>,
}

impl MacdAlphaModel {
    #[sdk_new]
    pub fn new(
        fast_period: usize,
        slow_period: usize,
        signal_period: usize,
        period_days: i64,
    ) -> Self {
        Self {
            inner: create_macd_alpha_model(fast_period, slow_period, signal_period, period_days),
        }
    }
}

#[sdk_bind(py_name = "RsiAlphaModel")]
pub struct RsiAlphaModel {
    #[allow(dead_code)]
    inner: Box<dyn IAlphaModel>,
}

impl RsiAlphaModel {
    #[sdk_new]
    pub fn new(period: usize, period_days: i64) -> Self {
        Self {
            inner: create_rsi_alpha_model(period, period_days),
        }
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
#[sdk_bind(py_name = "Insight")]
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
    #[sdk_static(alias = "Price")]
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

    #[sdk_getter(py_name = "price")]
    pub fn price_value(&self) -> Option<f64> {
        self.magnitude
    }

    #[sdk_getter(alias = "Symbol")]
    pub fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    #[sdk_getter(alias = "Direction")]
    pub fn direction(&self) -> InsightDirection {
        sdk_direction_from_alpha(self.direction)
    }

    #[sdk_getter]
    pub fn magnitude(&self) -> Option<f64> {
        self.magnitude
    }

    #[sdk_getter]
    pub fn confidence(&self) -> Option<f64> {
        self.confidence
    }

    #[sdk_getter(alias = "SourceModel")]
    pub fn source_model(&self) -> String {
        self.source_model.clone()
    }

    #[sdk_getter]
    pub fn score_direction(&self) -> Option<f64> {
        self.score_direction
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
#[sdk_bind(py_name = "PortfolioTarget")]
pub struct PortfolioTargetProjection {
    pub symbol: Symbol,
    pub quantity: Option<f64>,
    pub percent: Option<f64>,
    pub tag: String,
}

impl PortfolioTargetProjection {
    #[sdk_new]
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

    #[sdk_static(alias = "Percent")]
    pub fn percent(symbol: Symbol, percent: f64) -> Self {
        Self {
            symbol,
            quantity: None,
            percent: Some(percent),
            tag: String::new(),
        }
    }
}

pub fn portfolio_target_from_projection(
    projection: &PortfolioTargetProjection,
    portfolio_value: Decimal,
    prices: &HashMap<String, Decimal>,
) -> Option<PortfolioTarget> {
    if let Some(percent) = projection.percent {
        let percent = Decimal::from_f64(percent)?;
        let price = prices
            .get(projection.symbol.value.as_ref())
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

    #[test]
    fn portfolio_target_projection_converts_percent_with_prices() {
        let symbol = Symbol::create_equity("SPY", &lean_core::Market::usa());
        let projection = PortfolioTargetProjection {
            symbol: symbol.clone(),
            quantity: None,
            percent: Some(0.5),
            tag: "half".to_string(),
        };
        let prices = HashMap::from([(symbol.value.to_string(), Decimal::from(100))]);

        let target =
            portfolio_target_from_projection(&projection, Decimal::from(10_000), &prices).unwrap();

        assert_eq!(target.symbol, symbol);
        assert_eq!(target.quantity, Decimal::from(50));
        assert_eq!(target.tag, "half");
    }
}
