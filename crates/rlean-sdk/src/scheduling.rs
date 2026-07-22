use crate::universe::{DateRule, DateRuleHandle, TimeRule, TimeRuleHandle};
use rlean_algorithm::lifecycle::{AlgorithmRuntimeServices, ScheduledEventRegistrationRequest};
use std::sync::Arc;

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "ScheduleManager"))]
pub struct ScheduleManagerHandle {
    runtime_services: Arc<dyn AlgorithmRuntimeServices>,
}

impl ScheduleManagerHandle {
    pub fn new(runtime_services: Arc<dyn AlgorithmRuntimeServices>) -> Self {
        Self { runtime_services }
    }

    pub fn on(
        &self,
        name: impl Into<String>,
        date_rule: DateRuleHandle,
        time_rule: TimeRuleHandle,
        callback: impl FnMut() -> Result<(), String> + Send + 'static,
    ) {
        self.runtime_services
            .register_scheduled_event(ScheduledEventRegistrationRequest {
                name: name.into(),
                date_rule: convert_date_rule(date_rule.kind),
                time_rule: convert_time_rule(time_rule.kind),
                callback: Box::new(callback),
            });
    }
}

fn convert_date_rule(rule: DateRule) -> rlean_scheduling::DateRule {
    match rule {
        DateRule::EveryDay => rlean_scheduling::DateRule::EveryDay,
    }
}

fn convert_time_rule(rule: TimeRule) -> rlean_scheduling::TimeRule {
    match rule {
        TimeRule::At { hour, minute } => rlean_scheduling::TimeRule::At { hour, minute },
        TimeRule::AfterMarketOpen { minutes_after_open } => {
            rlean_scheduling::TimeRule::AfterMarketOpen { minutes_after_open }
        }
        TimeRule::BeforeMarketClose {
            symbol,
            minutes_before_close,
            extended_market_close,
        } => rlean_scheduling::TimeRule::BeforeMarketClose {
            symbol,
            minutes_before_close,
            extended_market_close,
        },
        TimeRule::EveryResolution => rlean_scheduling::TimeRule::EveryResolution,
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl ScheduleManagerHandle {
    #[pyo3(name = "on")]
    fn py_on(
        &self,
        date_rule: DateRuleHandle,
        time_rule: TimeRuleHandle,
        callback: pyo3::Py<pyo3::PyAny>,
    ) {
        use pyo3::types::PyAnyMethods;
        let name = PythonCallbackName::new(&callback);
        self.on(name, date_rule, time_rule, move || {
            pyo3::Python::attach(|py| callback.bind(py).call0().map(|_| ()))
                .map_err(|error| error.to_string())
        });
    }
}

#[cfg(feature = "python")]
struct PythonCallbackName;

#[cfg(feature = "python")]
impl PythonCallbackName {
    fn new(callback: &pyo3::Py<pyo3::PyAny>) -> String {
        use pyo3::types::PyAnyMethods;
        pyo3::Python::attach(|py| {
            callback
                .bind(py)
                .getattr("__qualname__")
                .and_then(|value| value.extract::<String>())
                .unwrap_or_else(|_| "ScheduledEvent".to_string())
        })
    }
}
