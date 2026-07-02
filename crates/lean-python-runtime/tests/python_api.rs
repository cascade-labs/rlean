#[cfg(test)]
mod tests {
    use lean_algorithm::lifecycle::NullHistoryService;
    use lean_algorithm::qc_algorithm::QcAlgorithm;
    use lean_python_runtime::AlgorithmImports;
    use lean_sdk::algorithm::{AlgorithmConstructionContext, AlgorithmHandle};
    use pyo3::prelude::*;
    use rust_decimal_macros::dec;
    use std::sync::{Arc, Mutex};

    fn init_python() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            pyo3::append_to_inittab!(AlgorithmImports);
            pyo3::Python::initialize();
        });
    }

    fn run_python(code: &str) {
        init_python();
        let state = Arc::new(Mutex::new(QcAlgorithm::new("Algorithm", dec!(100000))));
        let runtime_context = lean_engine::AlgorithmRuntimeContext::with_history_service(
            Arc::new(NullHistoryService),
            std::collections::HashMap::new(),
        );
        let context = AlgorithmConstructionContext::new_with_runtime_services(
            state,
            Arc::new(runtime_context),
        );
        AlgorithmHandle::with_default_context(context, || {
            Python::attach(|py| {
                let code = std::ffi::CString::new(code).unwrap();
                py.run(code.as_c_str(), None, None).unwrap();
            });
        });
    }

    #[test]
    fn algorithm_imports_exposes_expected_sdk_surface() {
        run_python(
            r#"
from AlgorithmImports import (
    QCAlgorithm,
    Symbol,
    Resolution,
    SecurityType,
    OrderStatus,
    OptionRight,
    UniverseSettings,
    ChartCollection,
)

assert QCAlgorithm is not None
assert Symbol is not None
assert Resolution.Daily is not None
assert SecurityType.Equity is not None
assert OrderStatus.Canceled is not None
assert OptionRight.Put is not None
assert UniverseSettings is not None
assert ChartCollection is not None
"#,
        );
    }

    #[test]
    fn generated_api_is_snake_case_not_csharp_case() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

algo = QCAlgorithm()
assert hasattr(algo, "set_cash"), "missing set_cash"
assert hasattr(algo, "add_equity"), "missing add_equity"
assert hasattr(algo, "market_order"), "missing market_order"
assert hasattr(algo, "history"), "missing history"

portfolio = algo.portfolio
assert hasattr(portfolio, "total_portfolio_value")
assert hasattr(portfolio, "hold_stock")

spy = algo.add_equity("SPY", Resolution.Daily).symbol
ticket = algo.market_order(spy, 1.0, None, False)
assert hasattr(ticket, "average_fill_price")
assert hasattr(ticket, "stop_price")
"#,
        );
    }

    #[test]
    fn generated_algorithm_history_binding_is_exposed() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm, Resolution, Symbol

algo = QCAlgorithm()
spy = Symbol.create_equity("SPY", None)
assert hasattr(algo, "history"), "missing history"
history = algo.history(spy, 1, Resolution.Daily)
assert isinstance(history, dict)
"#,
        );
    }

    #[test]
    fn algorithm_parameters_are_available_from_python() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm

algo = QCAlgorithm()
assert algo.get_parameter("missing", "fallback") == "fallback"
assert algo.get_parameter("missing", None) is None
algo.set_parameter("lookback", "63")
assert algo.get_parameter("lookback", None) == "63"
"#,
        );
    }

    #[test]
    fn algorithm_cash_security_and_order_helpers_work_from_python() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm, Resolution, TimeInForce

algo = QCAlgorithm()
algo.set_cash(12345.67)
assert abs(algo.cash - 12345.67) < 1e-9
assert abs(algo.portfolio.cash - 12345.67) < 1e-9
assert abs(algo.portfolio.total_portfolio_value - 12345.67) < 1e-9

algo.add_cash(54.33)
assert abs(algo.cash - 12400.0) < 1e-9

security = algo.add_equity("SPY", Resolution.Daily)
spy = security.symbol
assert spy.value == "SPY"
assert spy.ticker == "SPY"
assert algo.has_security(spy)
assert algo.portfolio[spy].invested is False

market = algo.market_order(spy, 10.0, TimeInForce.Day, False)
assert market.symbol.value == "SPY"
assert abs(market.quantity - 10.0) < 1e-9

limit = algo.limit_order(spy, -5.0, 401.25, None, True, False)
assert abs(limit.quantity + 5.0) < 1e-9
assert abs(limit.limit_price - 401.25) < 1e-9

stop = algo.stop_market_order(spy, 3.0, 399.5, None, False)
assert abs(stop.quantity - 3.0) < 1e-9
assert abs(stop.stop_price - 399.5) < 1e-9
"#,
        );
    }

    #[test]
    fn market_order_accepts_lean_default_arguments_from_python() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

algo = QCAlgorithm()
option = algo.add_option("SPY", Resolution.Daily)
symbol = option.symbol
ticket = algo.market_order(symbol, -1.0)
assert ticket.symbol == symbol
assert abs(ticket.quantity + 1.0) < 1e-9
"#,
        );
    }

    #[test]
    fn enum_aliases_and_sdk_objects_match_python_parity() {
        run_python(
            r#"
from AlgorithmImports import (
    ChartCollection,
    OptionRight,
    OrderStatus,
    Resolution,
    Symbol,
    UniverseSettings,
)

assert Resolution.Daily == Resolution.DAILY
assert OptionRight.Put == OptionRight.PUT
assert OrderStatus.Canceled == OrderStatus.CANCELED

settings = UniverseSettings()
settings.set_resolution(Resolution.Hour)
assert settings.resolution == Resolution.Hour

spy = Symbol.create_equity("spy", None)
assert spy.value == "SPY"
assert spy.ticker == "SPY"

charts = ChartCollection()
charts.plot("Strategy", "Equity", "2024-01-01", 100.0)
assert hasattr(charts, "plot")
assert not hasattr(charts, "Plot")
"#,
        );
    }

    #[test]
    fn symbols_compare_and_hash_by_lean_identity_from_python() {
        run_python(
            r#"
from AlgorithmImports import Insight, InsightDirection, Symbol

spy_a = Symbol.create_equity("spy", None)
spy_b = Symbol.create_equity("SPY", None)
qqq = Symbol.create_equity("QQQ", None)

assert spy_a == spy_b
assert not (spy_a != spy_b)
assert spy_a != qqq
assert hash(spy_a) == hash(spy_b)

states = {spy_a: "state"}
assert states[spy_b] == "state"
assert spy_b in {spy_a}

insight = Insight.Price(spy_b, 1, InsightDirection.Up)
assert insight.symbol == spy_a
assert insight.symbol in {spy_a}
"#,
        );
    }

    #[test]
    fn lean_indicator_helper_methods_are_exposed_in_snake_case() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm, Resolution, MovingAverageType

algo = QCAlgorithm()
spy = algo.add_equity("SPY", Resolution.Daily).symbol
vix = algo.add_data("cboe_vix", "VIX", Resolution.Daily).symbol
assert vix.value == "VIX"
assert algo.has_security(vix)
option = algo.add_option("SPY", Resolution.Daily)
assert option.symbol is not None
option.set_filter(-10, 10, 20, 45)
assert hasattr(option, "set_filter")

for name in [
    "sma",
    "ema",
    "rsi",
    "momp",
    "std",
    "bb",
    "macd",
    "identity",
    "register_indicator",
    "warm_up_indicator",
]:
    assert hasattr(algo, name), f"missing QCAlgorithm.{name}"

for csharp_name in ["SMA", "EMA", "RSI", "MOMP", "STD", "BB", "MACD", "RegisterIndicator"]:
    assert not hasattr(algo, csharp_name), f"unexpected C# casing {csharp_name}"

sma = algo.sma(spy, 14, Resolution.Daily)
ema = algo.ema(spy, 14, Resolution.Daily)
rsi = algo.rsi(spy, 14, MovingAverageType.Wilders, Resolution.Daily)
momp = algo.momp(spy, 14, Resolution.Daily)
std = algo.std(spy, 14, Resolution.Daily)
assert sma is not None
assert ema is not None
assert rsi is not None
assert momp is not None
assert std is not None
algo.set_warm_up(3, Resolution.Daily)
"#,
        );
    }

    #[test]
    fn insight_price_accepts_timedelta_and_weight() {
        run_python(
            r#"
from datetime import timedelta
from AlgorithmImports import Insight, InsightDirection, Resolution, QCAlgorithm

algo = QCAlgorithm()
sym = algo.add_equity("SPY", Resolution.Daily).symbol
insight = Insight.price(
    sym,
    timedelta(days=21),
    InsightDirection.Up,
    0.05,
    1.0,
    "test_model",
    None,
)
assert insight.magnitude == 0.05
assert insight.source_model == "test_model"
assert insight.direction == InsightDirection.Up
"#,
        );
    }

    #[test]
    fn framework_registration_helpers_are_snake_case() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm, AlphaModel, EqualWeightingPortfolioConstructionModel, ImmediateExecutionModel, NullRiskManagementModel

class DummyAlpha(AlphaModel):
    def Update(self, algorithm, data):
        return []

algo = QCAlgorithm()
alpha = DummyAlpha()
algo.add_alpha(alpha)
algo.set_portfolio_construction(EqualWeightingPortfolioConstructionModel())
algo.set_execution(ImmediateExecutionModel())
algo.set_risk_management(NullRiskManagementModel())
assert hasattr(algo, "insights")
assert hasattr(algo, "securities")
assert hasattr(algo, "settings")
assert not hasattr(algo, "AddAlpha")
assert not hasattr(algo, "SetPortfolioConstruction")
"#,
        );
    }

    #[test]
    fn framework_model_factories_are_available_from_python() {
        run_python(
            r#"
from AlgorithmImports import (
    InsightWeightingPortfolioConstructionModel,
    EqualWeightingPortfolioConstructionModel,
    MeanVarianceOptimizationPortfolioConstructionModel,
    MaximumSharpeRatioPortfolioConstructionModel,
    ImmediateExecutionModel,
    NullExecutionModel,
    VWAPExecutionModel,
    StandardDeviationExecutionModel,
    NullRiskManagementModel,
    MaximumDrawdownPercentPerSecurity,
    TrailingStopRiskManagementModel,
    ConstantAlphaModel,
    EmaCrossAlphaModel,
    HistoricalReturnsAlphaModel,
    MacdAlphaModel,
    RsiAlphaModel,
)

models = [
    InsightWeightingPortfolioConstructionModel(),
    EqualWeightingPortfolioConstructionModel(),
    MeanVarianceOptimizationPortfolioConstructionModel(),
    MaximumSharpeRatioPortfolioConstructionModel(),
    ImmediateExecutionModel(),
    NullExecutionModel(),
    VWAPExecutionModel(),
    StandardDeviationExecutionModel(20, 2.0),
    NullRiskManagementModel(),
    MaximumDrawdownPercentPerSecurity(0.05),
    TrailingStopRiskManagementModel(0.1),
    ConstantAlphaModel("up", 1, 0.01),
    EmaCrossAlphaModel(12, 26, 1),
    HistoricalReturnsAlphaModel(20, 1),
    MacdAlphaModel(12, 26, 9, 1),
    RsiAlphaModel(14, 1),
]
assert all(model is not None for model in models)
"#,
        );
    }

    #[test]
    fn framework_and_lifecycle_hooks_are_python_override_points() {
        run_python(
            r#"
from AlgorithmImports import QCAlgorithm

class Strategy(QCAlgorithm):
    def initialize(self):
        self.initialized = True

    def on_data(self, slice):
        self.last_slice = slice

    def on_warmup_finished(self):
        self.warmup_finished = True

    def on_end_of_day(self, symbol=None):
        self.eod_symbol = symbol

    def on_end_of_algorithm(self):
        self.ended = True

    def on_order_event(self, order_event):
        self.last_order_event = order_event

    def on_securities_changed(self, changes):
        self.last_changes = changes

    def on_margin_call(self, requests):
        return requests

    def on_margin_call_warning(self):
        self.margin_warning = True

    def on_framework_data(self, slice):
        self.framework_slice = slice

    def on_assignment_order_event(self, assignment_event):
        self.assignment_event = assignment_event

    def on_otm_expiry(self, expiry_event):
        self.expiry_event = expiry_event

strategy = Strategy()
for name in [
    "initialize",
    "on_data",
    "on_warmup_finished",
    "on_end_of_day",
    "on_end_of_algorithm",
    "on_order_event",
    "on_securities_changed",
    "on_margin_call",
    "on_margin_call_warning",
    "on_framework_data",
    "on_assignment_order_event",
    "on_otm_expiry",
]:
    assert hasattr(strategy, name), f"missing lifecycle hook {name}"
"#,
        );
    }

    #[test]
    fn quantbook_research_surface_matches_lean_python_names() {
        run_python(
            r#"
from AlgorithmImports import QuantBook, Resolution

qb = QuantBook()
for name in [
    "set_start_date",
    "set_end_date",
    "add_equity",
    "add_option",
    "history",
    "history_range",
    "indicator",
    "indicator_frame",
    "option_chain",
    "get_last_price",
]:
    assert hasattr(qb, name), f"missing QuantBook.{name}"

assert not hasattr(qb, "SetStartDate")
assert not hasattr(qb, "History")
"#,
        );
    }
}
