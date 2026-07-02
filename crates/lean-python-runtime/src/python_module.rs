use pyo3::prelude::*;

#[pymodule]
#[pyo3(name = "AlgorithmImports")]
pub fn algorithm_imports(m: &Bound<'_, PyModule>) -> PyResult<()> {
    register_algorithm(m)?;
    register_charting(m)?;
    register_data(m)?;
    register_framework(m)?;
    register_indicators(m)?;
    register_options(m)?;
    register_orders(m)?;
    register_portfolio(m)?;
    register_research(m)?;
    register_securities(m)?;
    register_types(m)?;
    register_universe(m)?;
    Ok(())
}

pub use algorithm_imports as AlgorithmImports;

fn register_algorithm(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::algorithm::AlgorithmHandle>()?;
    m.add_class::<lean_sdk::algorithm::BrokerageModelHandle>()?;
    m.add_class::<lean_sdk::algorithm::FuncSecuritySeederHandle>()?;
    m.add_class::<lean_sdk::algorithm::BrokerageModelSecurityInitializerHandle>()?;
    Ok(())
}

fn register_charting(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::charting::ChartCollectionHandle>()?;
    Ok(())
}

fn register_data(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::data::SliceView>()?;
    m.add_class::<lean_sdk::data::OptionChainsView>()?;
    m.add_class::<lean_sdk::data::TradeBarsView>()?;
    m.add_class::<lean_sdk::data::TradeBarView>()?;
    m.add_class::<lean_sdk::data::BarView>()?;
    m.add_class::<lean_sdk::data::QuoteBarsView>()?;
    m.add_class::<lean_sdk::data::QuoteBarView>()?;
    m.add_class::<lean_sdk::data::TicksView>()?;
    m.add_class::<lean_sdk::data::TickView>()?;
    m.add_class::<lean_sdk::data::CustomDataView>()?;
    m.add_class::<lean_sdk::data::CustomDataPointView>()?;
    Ok(())
}

fn register_framework(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::framework::InsightDirection>()?;
    m.add_class::<lean_sdk::framework::PortfolioBiasView>()?;
    m.add_class::<lean_sdk::framework::AlphaModel>()?;
    m.add_class::<lean_sdk::framework::ExecutionModel>()?;
    m.add_class::<lean_sdk::framework::PortfolioConstructionModel>()?;
    m.add_class::<lean_sdk::framework::RiskManagementModel>()?;
    m.add_class::<lean_sdk::framework::InsightWeightingPortfolioConstructionModel>()?;
    m.add_class::<lean_sdk::framework::EqualWeightingPortfolioConstructionModel>()?;
    m.add_class::<lean_sdk::framework::MeanVarianceOptimizationPortfolioConstructionModel>()?;
    m.add_class::<lean_sdk::framework::MaximumSharpeRatioPortfolioConstructionModel>()?;
    m.add_class::<lean_sdk::framework::ImmediateExecutionModel>()?;
    m.add_class::<lean_sdk::framework::NullExecutionModel>()?;
    m.add_class::<lean_sdk::framework::VwapExecutionModel>()?;
    m.add_class::<lean_sdk::framework::StandardDeviationExecutionModel>()?;
    m.add_class::<lean_sdk::framework::NullRiskManagementModel>()?;
    m.add_class::<lean_sdk::framework::MaximumDrawdownPercentPerSecurity>()?;
    m.add_class::<lean_sdk::framework::TrailingStopRiskManagementModel>()?;
    m.add_class::<lean_sdk::framework::ConstantAlphaModel>()?;
    m.add_class::<lean_sdk::framework::EmaCrossAlphaModel>()?;
    m.add_class::<lean_sdk::framework::HistoricalReturnsAlphaModel>()?;
    m.add_class::<lean_sdk::framework::MacdAlphaModel>()?;
    m.add_class::<lean_sdk::framework::RsiAlphaModel>()?;
    m.add_class::<lean_sdk::framework::InsightProjection>()?;
    m.add_class::<lean_sdk::framework::PortfolioTargetProjection>()?;
    m.add_class::<lean_sdk::python_framework::InsightCollectionHandle>()?;
    Ok(())
}

fn register_indicators(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::indicators::IndicatorDataPointView>()?;
    m.add_class::<lean_sdk::indicators::IndicatorResultView>()?;
    m.add_class::<lean_sdk::indicators::SimpleMovingAverage>()?;
    m.add_class::<lean_sdk::indicators::ExponentialMovingAverage>()?;
    m.add_class::<lean_sdk::indicators::RelativeStrengthIndex>()?;
    m.add_class::<lean_sdk::indicators::MomentumPercentIndicator>()?;
    m.add_class::<lean_sdk::indicators::StandardDeviationIndicator>()?;
    m.add_class::<lean_sdk::indicators::BollingerBandsIndicator>()?;
    m.add_class::<lean_sdk::indicators::MacdIndicator>()?;
    m.add_class::<lean_sdk::indicators::AverageTrueRange>()?;
    m.add_class::<lean_sdk::indicators::IdentityIndicator>()?;
    Ok(())
}

fn register_options(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::options::GreeksView>()?;
    m.add_class::<lean_sdk::options::UnderlyingView>()?;
    m.add_class::<lean_sdk::options::OptionContractView>()?;
    m.add_class::<lean_sdk::options::OptionChainView>()?;
    Ok(())
}

fn register_orders(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::orders::OrderTicketHandle>()?;
    m.add_class::<lean_sdk::orders::OrderEventView>()?;
    Ok(())
}

fn register_portfolio(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::portfolio::SecurityHoldingView>()?;
    m.add_class::<lean_sdk::portfolio::PortfolioView>()?;
    Ok(())
}

fn register_research(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::research::ResearchBook>()?;
    Ok(())
}

fn register_securities(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::securities::SecurityHandle>()?;
    m.add_class::<lean_sdk::securities::OptionSecurityHandle>()?;
    m.add_class::<lean_sdk::securities::SecurityExchangeView>()?;
    m.add_class::<lean_sdk::securities::ExchangeHoursHandle>()?;
    m.add_class::<lean_sdk::securities::SecurityManagerHandle>()?;
    m.add_class::<lean_sdk::securities::SymbolHandle>()?;
    m.add_class::<lean_sdk::securities::AlgorithmSettingsHandle>()?;
    Ok(())
}

fn register_types(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::types::MarketConstants>()?;
    m.add_class::<lean_sdk::types::HyperliquidUniverseConstants>()?;
    m.add_class::<lean_sdk::types::Resolution>()?;
    m.add_class::<lean_sdk::types::SecurityType>()?;
    m.add_class::<lean_sdk::types::DataNormalizationMode>()?;
    m.add_class::<lean_sdk::types::TimeInForce>()?;
    m.add_class::<lean_sdk::types::OptionRight>()?;
    m.add_class::<lean_sdk::types::OptionStyle>()?;
    m.add_class::<lean_sdk::types::AccountType>()?;
    m.add_class::<lean_sdk::types::BrokerageName>()?;
    m.add_class::<lean_sdk::types::OrderType>()?;
    m.add_class::<lean_sdk::types::OrderStatus>()?;
    m.add_class::<lean_sdk::types::OrderDirection>()?;
    m.add_class::<lean_sdk::types::MovingAverageType>()?;
    Ok(())
}

fn register_universe(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<lean_sdk::universe::UniverseSettingsHandle>()?;
    m.add_class::<lean_sdk::universe::DateRuleHandle>()?;
    m.add_class::<lean_sdk::universe::DateRulesHandle>()?;
    m.add_class::<lean_sdk::universe::TimeRuleHandle>()?;
    m.add_class::<lean_sdk::universe::TimeRulesHandle>()?;
    m.add_class::<lean_sdk::universe::ScheduledUniverseHandle>()?;
    m.add_class::<lean_sdk::universe::SecurityChangesView>()?;
    Ok(())
}
