use crate::py_types::{PyResolution, PySecurity, PySymbol};
use lean_algorithm::algorithm::SecurityChanges;
use lean_core::{Market, Resolution, Symbol};
use pyo3::prelude::*;
use pyo3::types::{PyList, PyTuple};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct UniverseSettingsState {
    pub resolution: Resolution,
    pub leverage: f64,
    pub fill_forward: bool,
    pub extended_market_hours: bool,
    pub minimum_time_in_universe_secs: f64,
}

impl Default for UniverseSettingsState {
    fn default() -> Self {
        Self {
            resolution: Resolution::Daily,
            leverage: 1.0,
            fill_forward: true,
            extended_market_hours: false,
            minimum_time_in_universe_secs: 0.0,
        }
    }
}

#[pyclass(name = "UniverseSettings")]
#[derive(Debug, Clone)]
pub struct PyUniverseSettings {
    pub inner: Arc<Mutex<UniverseSettingsState>>,
}

impl PyUniverseSettings {
    pub fn new_shared() -> Self {
        Self {
            inner: Arc::new(Mutex::new(UniverseSettingsState::default())),
        }
    }

    pub fn snapshot(&self) -> UniverseSettingsState {
        self.inner.lock().unwrap().clone()
    }
}

#[pymethods]
impl PyUniverseSettings {
    #[new]
    fn new() -> Self {
        Self::new_shared()
    }

    #[getter]
    fn resolution(&self) -> PyResolution {
        match self.inner.lock().unwrap().resolution {
            Resolution::Tick => PyResolution::Tick,
            Resolution::Second => PyResolution::Second,
            Resolution::Minute => PyResolution::Minute,
            Resolution::Hour => PyResolution::Hour,
            Resolution::Daily => PyResolution::Daily,
        }
    }

    #[setter]
    fn set_resolution(&self, value: PyResolution) {
        self.inner.lock().unwrap().resolution = value.into();
    }

    #[getter]
    fn leverage(&self) -> f64 {
        self.inner.lock().unwrap().leverage
    }

    #[setter]
    fn set_leverage(&self, value: f64) {
        self.inner.lock().unwrap().leverage = value;
    }

    #[getter]
    fn fill_forward(&self) -> bool {
        self.inner.lock().unwrap().fill_forward
    }

    #[setter]
    fn set_fill_forward(&self, value: bool) {
        self.inner.lock().unwrap().fill_forward = value;
    }

    #[getter]
    fn extended_market_hours(&self) -> bool {
        self.inner.lock().unwrap().extended_market_hours
    }

    #[setter]
    fn set_extended_market_hours(&self, value: bool) {
        self.inner.lock().unwrap().extended_market_hours = value;
    }

    #[getter]
    fn minimum_time_in_universe(&self) -> f64 {
        self.inner.lock().unwrap().minimum_time_in_universe_secs
    }

    #[setter]
    fn set_minimum_time_in_universe(&self, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let seconds = if let Ok(seconds) = value.extract::<f64>() {
            seconds
        } else if let Ok(td) = value.extract::<chrono::Duration>() {
            td.num_milliseconds() as f64 / 1000.0
        } else {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "minimum_time_in_universe expects seconds or datetime.timedelta",
            ));
        };
        self.inner.lock().unwrap().minimum_time_in_universe_secs = seconds.max(0.0);
        Ok(())
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'UniverseSettings' object has no attribute '{name}'"
        )))
    }
}

#[derive(Debug, Clone)]
enum DateRuleKind {
    EveryDay,
}

#[pyclass(name = "DateRule")]
#[derive(Debug, Clone)]
pub struct PyDateRule {
    kind: DateRuleKind,
}

#[pyclass(name = "DateRules")]
#[derive(Debug, Clone, Default)]
pub struct PyDateRules {}

#[pymethods]
impl PyDateRules {
    #[new]
    fn new() -> Self {
        Self {}
    }

    fn every_day(&self) -> PyDateRule {
        PyDateRule {
            kind: DateRuleKind::EveryDay,
        }
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'DateRules' object has no attribute '{name}'"
        )))
    }
}

#[derive(Debug, Clone)]
enum TimeRuleKind {
    At { hour: u32, minute: u32 },
    AfterMarketOpen { minutes_after_open: i64 },
    EveryResolution,
}

#[pyclass(name = "TimeRule")]
#[derive(Debug, Clone)]
pub struct PyTimeRule {
    kind: TimeRuleKind,
}

#[pyclass(name = "TimeRules")]
#[derive(Debug, Clone, Default)]
pub struct PyTimeRules {}

#[pymethods]
impl PyTimeRules {
    #[new]
    fn new() -> Self {
        Self {}
    }

    #[pyo3(signature = (hour, minute, _second=0))]
    fn at(&self, hour: u32, minute: u32, _second: u32) -> PyTimeRule {
        PyTimeRule {
            kind: TimeRuleKind::At { hour, minute },
        }
    }

    #[pyo3(signature = (_symbol=None, minutes_after_open=0))]
    fn after_market_open(
        &self,
        _symbol: Option<&Bound<'_, PyAny>>,
        minutes_after_open: i64,
    ) -> PyTimeRule {
        PyTimeRule {
            kind: TimeRuleKind::AfterMarketOpen { minutes_after_open },
        }
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'TimeRules' object has no attribute '{name}'"
        )))
    }
}

#[derive(Clone)]
struct UniverseMember {
    symbol: Symbol,
    added_ns: i64,
}

#[pyclass(name = "ScheduledUniverse")]
pub struct PyScheduledUniverse {
    date_rule: PyDateRule,
    time_rule: PyTimeRule,
    selector: Py<PyAny>,
    settings: UniverseSettingsState,
    selected: HashMap<u64, UniverseMember>,
}

impl PyScheduledUniverse {
    pub fn user_defined(
        selector: Py<PyAny>,
        resolution: Resolution,
        settings: UniverseSettingsState,
    ) -> Self {
        let mut settings = settings;
        settings.resolution = resolution;
        Self {
            date_rule: PyDateRule {
                kind: DateRuleKind::EveryDay,
            },
            time_rule: PyTimeRule {
                kind: TimeRuleKind::EveryResolution,
            },
            selector,
            settings,
            selected: HashMap::new(),
        }
    }

    pub fn settings(&self) -> UniverseSettingsState {
        self.settings.clone()
    }

    pub fn should_trigger(&self, utc_ns: i64, resolution: Resolution) -> bool {
        let dt = ns_to_utc_datetime(utc_ns);
        match self.date_rule.kind {
            DateRuleKind::EveryDay => {}
        }
        match self.time_rule.kind {
            TimeRuleKind::EveryResolution => resolution == self.settings.resolution,
            TimeRuleKind::At { hour, minute } => dt.hour() == hour && dt.minute() == minute,
            TimeRuleKind::AfterMarketOpen { minutes_after_open } => {
                let open = chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap()
                    + chrono::Duration::minutes(minutes_after_open);
                dt.time().hour() == open.hour() && dt.time().minute() == open.minute()
            }
        }
    }

    pub fn select(&mut self, py: Python<'_>, utc_ns: i64) -> PyResult<SecurityChanges> {
        let dt = py_datetime_from_ns(py, utc_ns)?;
        let result = self.selector.call1(py, (dt,))?;
        if result.bind(py).is_none() {
            return Ok(SecurityChanges::empty());
        }
        let symbols = extract_symbols(result.bind(py))?;
        let mut next = HashMap::new();
        for symbol in symbols {
            next.insert(symbol.id.sid, symbol);
        }

        let mut added = Vec::new();
        for (sid, symbol) in &next {
            if !self.selected.contains_key(sid) {
                added.push(symbol.clone());
            }
        }

        let mut removed = Vec::new();
        let min_secs = self.settings.minimum_time_in_universe_secs;
        self.selected.retain(|sid, member| {
            if next.contains_key(sid) {
                true
            } else if can_remove_member(member.added_ns, utc_ns, min_secs) {
                removed.push(member.symbol.clone());
                false
            } else {
                true
            }
        });

        for symbol in added.iter().cloned() {
            self.selected.insert(
                symbol.id.sid,
                UniverseMember {
                    symbol,
                    added_ns: utc_ns,
                },
            );
        }

        Ok(SecurityChanges { added, removed })
    }
}

#[pymethods]
impl PyScheduledUniverse {
    #[new]
    #[pyo3(signature = (date_rule, time_rule, selector, settings=None))]
    fn new(
        date_rule: PyDateRule,
        time_rule: PyTimeRule,
        selector: Py<PyAny>,
        settings: Option<PyUniverseSettings>,
    ) -> Self {
        Self {
            date_rule,
            time_rule,
            selector,
            settings: settings.map(|s| s.snapshot()).unwrap_or_default(),
            selected: HashMap::new(),
        }
    }

    fn get_trigger_times(
        &self,
        start: &Bound<'_, PyAny>,
        end: &Bound<'_, PyAny>,
    ) -> PyResult<Vec<Py<PyAny>>> {
        let start_ns = py_datetime_to_ns(start)?;
        let end_ns = py_datetime_to_ns(end)?;
        let mut out = Vec::new();
        let mut date = ns_to_utc_datetime(start_ns).date_naive();
        let end_dt = ns_to_utc_datetime(end_ns);
        while date <= end_dt.date_naive() {
            let Some(trigger) = trigger_time_for_date(date, &self.time_rule.kind) else {
                date += chrono::Duration::days(1);
                continue;
            };
            let ns = trigger.and_utc().timestamp_nanos_opt().unwrap_or(0);
            if ns > start_ns && ns < end_ns {
                out.push(py_datetime_from_ns(start.py(), ns)?);
            }
            date += chrono::Duration::days(1);
        }
        Ok(out)
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'ScheduledUniverse' object has no attribute '{name}'"
        )))
    }
}

#[pyclass(name = "SecurityChanges")]
#[derive(Clone)]
pub struct PySecurityChanges {
    added: Vec<PySecurity>,
    removed: Vec<PySecurity>,
}

impl PySecurityChanges {
    pub fn from_changes(changes: &SecurityChanges) -> Self {
        Self {
            added: changes
                .added
                .iter()
                .cloned()
                .map(|inner| PySecurity::from_symbol(PySymbol { inner }))
                .collect(),
            removed: changes
                .removed
                .iter()
                .cloned()
                .map(|inner| PySecurity::from_symbol(PySymbol { inner }))
                .collect(),
        }
    }
}

#[pymethods]
impl PySecurityChanges {
    #[new]
    fn new() -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    #[getter]
    fn added_securities(&self) -> Vec<PySecurity> {
        self.added.clone()
    }

    #[getter]
    fn removed_securities(&self) -> Vec<PySecurity> {
        self.removed.clone()
    }

    fn __getattr__(slf: &Bound<'_, Self>, name: &str) -> PyResult<Py<PyAny>> {
        let snake = crate::py_qc_algorithm::pascal_to_snake(name);
        if snake != name {
            if let Ok(attr) = slf.getattr(snake.as_str()) {
                return Ok(attr.unbind());
            }
        }
        Err(pyo3::exceptions::PyAttributeError::new_err(format!(
            "'SecurityChanges' object has no attribute '{name}'"
        )))
    }
}

fn extract_symbols(obj: &Bound<'_, PyAny>) -> PyResult<Vec<Symbol>> {
    if let Ok(list) = obj.cast::<PyList>() {
        return list.iter().map(|item| extract_symbol(&item)).collect();
    }
    if let Ok(tuple) = obj.cast::<PyTuple>() {
        return tuple.iter().map(|item| extract_symbol(&item)).collect();
    }
    Ok(vec![extract_symbol(obj)?])
}

fn extract_symbol(obj: &Bound<'_, PyAny>) -> PyResult<Symbol> {
    if let Ok(sym) = obj.cast::<PySymbol>() {
        return Ok(sym.get().inner.clone());
    }
    if let Ok(sec) = obj.cast::<PySecurity>() {
        return Ok(sec.get().inner.inner.clone());
    }
    if let Ok(ticker) = obj.extract::<String>() {
        return Ok(Symbol::create_equity(&ticker, &Market::usa()));
    }
    if let Ok(symbol_attr) = obj.getattr("symbol") {
        return extract_symbol(&symbol_attr);
    }
    if let Ok(symbol_attr) = obj.getattr("Symbol") {
        return extract_symbol(&symbol_attr);
    }
    Err(pyo3::exceptions::PyTypeError::new_err(
        "Universe selector must return Symbols, Securities, or ticker strings",
    ))
}

fn can_remove_member(added_ns: i64, utc_ns: i64, minimum_secs: f64) -> bool {
    if minimum_secs <= 0.0 {
        return true;
    }
    let elapsed_secs = (utc_ns - added_ns) as f64 / 1_000_000_000.0;
    elapsed_secs + rounding_epsilon(minimum_secs) >= minimum_secs
}

fn rounding_epsilon(minimum_secs: f64) -> f64 {
    if minimum_secs >= 86_400.0 {
        43_199.0
    } else if minimum_secs >= 3_600.0 {
        1_800.0
    } else if minimum_secs >= 60.0 {
        30.0
    } else {
        0.5
    }
}

fn trigger_time_for_date(
    date: chrono::NaiveDate,
    rule: &TimeRuleKind,
) -> Option<chrono::NaiveDateTime> {
    match rule {
        TimeRuleKind::At { hour, minute } => date.and_hms_opt(*hour, *minute, 0),
        TimeRuleKind::AfterMarketOpen { minutes_after_open } => {
            let open = chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap()
                + chrono::Duration::minutes(*minutes_after_open);
            Some(chrono::NaiveDateTime::new(date, open))
        }
        TimeRuleKind::EveryResolution => date.and_hms_opt(0, 0, 0),
    }
}

fn ns_to_utc_datetime(ns: i64) -> chrono::DateTime<chrono::Utc> {
    let secs = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).unwrap_or_default()
}

fn py_datetime_from_ns(py: Python<'_>, ns: i64) -> PyResult<Py<PyAny>> {
    let secs = ns / 1_000_000_000;
    let micros = (ns % 1_000_000_000) / 1_000;
    let timestamp = secs as f64 + micros as f64 / 1_000_000.0;
    Ok(py
        .import("datetime")?
        .getattr("datetime")?
        .call_method1("utcfromtimestamp", (timestamp,))?
        .unbind())
}

fn py_datetime_to_ns(value: &Bound<'_, PyAny>) -> PyResult<i64> {
    let timestamp: f64 = value.call_method0("timestamp")?.extract()?;
    Ok((timestamp * 1_000_000_000.0) as i64)
}

trait DateTimeParts {
    fn hour(&self) -> u32;
    fn minute(&self) -> u32;
}

impl DateTimeParts for chrono::DateTime<chrono::Utc> {
    fn hour(&self) -> u32 {
        chrono::Timelike::hour(self)
    }
    fn minute(&self) -> u32 {
        chrono::Timelike::minute(self)
    }
}

trait NaiveTimeParts {
    fn hour(&self) -> u32;
    fn minute(&self) -> u32;
}

impl NaiveTimeParts for chrono::NaiveTime {
    fn hour(&self) -> u32 {
        chrono::Timelike::hour(self)
    }
    fn minute(&self) -> u32 {
        chrono::Timelike::minute(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt_ns(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        let dt = chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap();
        dt.and_utc().timestamp_nanos_opt().unwrap()
    }

    #[test]
    fn scheduled_universe_time_triggered_does_not_return_past_times() {
        crate::test_python::init();
        Python::attach(|py| {
            let selector = py.eval(c"lambda time: []", None, None).unwrap().unbind();
            let universe = PyScheduledUniverse::new(
                PyDateRule {
                    kind: DateRuleKind::EveryDay,
                },
                PyTimeRule {
                    kind: TimeRuleKind::At {
                        hour: 12,
                        minute: 0,
                    },
                },
                selector,
                None,
            );
            let start = py_datetime_from_ns(py, dt_ns(2000, 1, 5, 15, 0)).unwrap();
            let end = py_datetime_from_ns(py, dt_ns(2000, 1, 10, 0, 0)).unwrap();
            let triggers = universe
                .get_trigger_times(start.bind(py), end.bind(py))
                .unwrap();
            let actual: Vec<String> = triggers
                .iter()
                .map(|t| {
                    t.bind(py)
                        .call_method0("isoformat")
                        .unwrap()
                        .extract::<String>()
                        .unwrap()
                })
                .collect();
            assert_eq!(
                actual,
                vec![
                    "2000-01-06T12:00:00",
                    "2000-01-07T12:00:00",
                    "2000-01-08T12:00:00",
                    "2000-01-09T12:00:00",
                ]
            );
        });
    }

    #[test]
    fn scheduled_universe_trigger_times_none_inside_window() {
        crate::test_python::init();
        Python::attach(|py| {
            let selector = py.eval(c"lambda time: []", None, None).unwrap().unbind();
            let universe = PyScheduledUniverse::new(
                PyDateRule {
                    kind: DateRuleKind::EveryDay,
                },
                PyTimeRule {
                    kind: TimeRuleKind::At {
                        hour: 12,
                        minute: 0,
                    },
                },
                selector,
                None,
            );
            let start = py_datetime_from_ns(py, dt_ns(2000, 1, 5, 15, 0)).unwrap();
            let end = py_datetime_from_ns(py, dt_ns(2000, 1, 5, 16, 0)).unwrap();
            let triggers = universe
                .get_trigger_times(start.bind(py), end.bind(py))
                .unwrap();
            assert!(triggers.is_empty());
        });
    }

    #[test]
    fn scheduled_universe_diffs_selected_symbols_like_lean() {
        crate::test_python::init();
        Python::attach(|py| {
            let selector = py
                .eval(
                    c"lambda time: ['SPY', 'AAPL'] if time.day == 1 else ['AAPL', 'MSFT']",
                    None,
                    None,
                )
                .unwrap()
                .unbind();
            let mut universe = PyScheduledUniverse::new(
                PyDateRule {
                    kind: DateRuleKind::EveryDay,
                },
                PyTimeRule {
                    kind: TimeRuleKind::At {
                        hour: 16,
                        minute: 0,
                    },
                },
                selector,
                None,
            );

            let first = universe.select(py, dt_ns(2024, 1, 1, 16, 0)).unwrap();
            assert_eq!(first.added.len(), 2);
            assert!(first.removed.is_empty());

            let unchanged = universe.select(py, dt_ns(2024, 1, 1, 16, 0)).unwrap();
            assert!(!unchanged.has_changes());

            let second = universe.select(py, dt_ns(2024, 1, 2, 16, 0)).unwrap();
            let added: Vec<_> = second.added.iter().map(|s| s.value.as_str()).collect();
            let removed: Vec<_> = second.removed.iter().map(|s| s.value.as_str()).collect();
            assert_eq!(added, vec!["MSFT"]);
            assert_eq!(removed, vec!["SPY"]);
        });
    }

    #[test]
    fn minimum_time_in_universe_uses_lean_rounding_thresholds() {
        let added = dt_ns(2018, 1, 1, 0, 0);

        assert!(!can_remove_member(added, added + 29_000_000_000, 30.0));
        assert!(can_remove_member(added, added + 29_500_000_000, 30.0));

        assert!(!can_remove_member(
            added,
            added + 29 * 60 * 1_000_000_000,
            30.0 * 60.0
        ));
        assert!(can_remove_member(
            added,
            added + (29 * 60 + 30) * 1_000_000_000,
            30.0 * 60.0
        ));

        assert!(!can_remove_member(
            added,
            added + 12 * 60 * 60 * 1_000_000_000,
            86_400.0
        ));
        assert!(can_remove_member(
            added,
            added + (12 * 60 * 60 + 1) * 1_000_000_000,
            86_400.0
        ));
    }
}
