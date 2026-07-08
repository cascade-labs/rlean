use crate::strategy_loader;
use anyhow::{Context, Result};
use lean_algorithm::algorithm::{AlgorithmStatus, DataDeliveryPayload, SecurityChanges};
use lean_algorithm::charting::ChartCollection;
use lean_algorithm::lifecycle::{
    AlgorithmRuntimeServices, AlgorithmServices, AlgorithmStateAccess, LifecycleBridge,
    OptionSubscription, UniverseSelection,
};
use lean_algorithm::qc_algorithm::QcAlgorithm;
use lean_alpha::AlphaAnalytics;
use lean_core::{DateTime, Price, Resolution, Symbol};
use lean_data::{
    CustomDataPoint, Delisting, Dividend, Split, SubscriptionDataConfig, SymbolChangedEvent,
};
use lean_options::OptionContract;
use lean_orders::{Order, OrderEvent, TransactionManager};
use lean_sdk::algorithm::{AlgorithmConstructionContext, AlgorithmHandle};
use lean_sdk::data::{SharedSliceFrame, SliceView};
use lean_sdk::orders::{
    assignment_expiry_event_view, margin_call_payload, margin_call_requests_from_orders,
    otm_expiry_event_view, OrderEventView,
};
use lean_sdk::securities::SymbolHandle;
use lean_sdk::universe::SecurityChangesView;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn debug_log(run_id: &str, hypothesis_id: &str, location: &str, data: serde_json::Value) {
    // #region agent log
    let payload = serde_json::json!({
        "sessionId": "3a8e9a",
        "runId": run_id,
        "hypothesisId": hypothesis_id,
        "location": location,
        "message": "ctrl-c backtest debug probe",
        "data": data,
        "timestamp": chrono::Utc::now().timestamp_millis()
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/Users/jfbrown/code/rlean/.cursor/debug-3a8e9a.log")
    {
        let _ = writeln!(file, "{payload}");
    }
    // #endregion
}

#[cfg(unix)]
fn debug_log_sigint_state(run_id: &str, hypothesis_id: &str, location: &str) {
    // #region agent log
    unsafe {
        let current = libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGINT, current);
        let mut blocked = false;
        let mut set = std::mem::zeroed();
        if libc::pthread_sigmask(libc::SIG_SETMASK, std::ptr::null(), &mut set) == 0 {
            blocked = libc::sigismember(&set, libc::SIGINT) == 1;
        }
        debug_log(
            run_id,
            hypothesis_id,
            location,
            serde_json::json!({
                "sigint_handler": current as usize,
                "sig_dfl": libc::SIG_DFL,
                "sig_ign": libc::SIG_IGN,
                "sigint_blocked": blocked
            }),
        );
    }
    // #endregion
}

#[cfg(not(unix))]
fn debug_log_sigint_state(run_id: &str, hypothesis_id: &str, location: &str) {
    debug_log(
        run_id,
        hypothesis_id,
        location,
        serde_json::json!({ "sigint_state": "unavailable" }),
    );
}

pub struct PythonAlgorithmBridge {
    strategy: Py<PyAny>,
    on_data_callback: Option<Py<PyAny>>,
    state: Arc<Mutex<QcAlgorithm>>,
    runtime_services: Arc<dyn AlgorithmRuntimeServices>,
    slice_frame: SharedSliceFrame,
    py_slice: Py<PyAny>,
    runtime_error: Option<String>,
}

impl PythonAlgorithmBridge {
    pub fn from_strategy(
        py: Python<'_>,
        strategy: Py<PyAny>,
        state: Arc<Mutex<QcAlgorithm>>,
        runtime_services: Arc<dyn AlgorithmRuntimeServices>,
    ) -> PyResult<Self> {
        let slice_frame = SharedSliceFrame::new();
        let py_slice = Py::new(py, SliceView::new(slice_frame.clone()))?.into_any();
        lean_sdk::python_framework::register_python_strategy_object(
            lean_sdk::python_framework::custom_universe_state_key(&state),
            strategy.clone_ref(py),
        );
        let strategy_ref = strategy.bind(py);
        let on_data_callback = strategy_ref
            .getattr("on_data")
            .or_else(|err| {
                if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) {
                    strategy_ref.getattr("OnData")
                } else {
                    Err(err)
                }
            })
            .ok()
            .map(|callback| callback.unbind());
        Ok(Self {
            strategy,
            on_data_callback,
            state,
            runtime_services,
            slice_frame,
            py_slice,
            runtime_error: None,
        })
    }

    fn call_method0_if_present(&self, method: &str) -> Result<()> {
        self.call_method0_if_present_any(&[method])
    }

    fn call_method0_if_present_any(&self, methods: &[&str]) -> Result<()> {
        Python::attach(|py| {
            let strategy = self.strategy.bind(py);
            for method in methods {
                match strategy.getattr(*method) {
                    Ok(callback) => {
                        callback.call0()?;
                        return Ok(());
                    }
                    Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {
                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok::<(), PyErr>(())
        })
        .with_context(|| format!("Python strategy callback {} failed", methods.join("/")))
    }

    fn call_on_data(&self, payload: DataDeliveryPayload) -> Result<()> {
        self.slice_frame.set_current(payload.slice);
        Python::attach(|py| {
            if let Some(callback) = &self.on_data_callback {
                callback.bind(py).call1((self.py_slice.clone_ref(py),))?;
            }
            Ok::<(), PyErr>(())
        })
        .context("Python strategy callback on_data failed")
    }

    fn call_order_event_callback(&self, methods: &[&str], event: OrderEventView) -> Result<()> {
        Python::attach(|py| {
            let strategy = self.strategy.bind(py);
            let py_event = Py::new(py, event)?.into_any();
            for method in methods {
                match strategy.getattr(*method) {
                    Ok(callback) => {
                        callback.call1((py_event.clone_ref(py),))?;
                        return Ok(());
                    }
                    Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {
                        continue;
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(())
        })
        .with_context(|| format!("Python strategy callback {} failed", methods.join("/")))
    }

    fn call_method1_if_present(&self, method: &str, arg: Py<PyAny>) -> Result<()> {
        Python::attach(|py| {
            let strategy = self.strategy.bind(py);
            match strategy.getattr(method) {
                Ok(callback) => {
                    callback.call1((arg.clone_ref(py),))?;
                }
                Err(err) if err.is_instance_of::<pyo3::exceptions::PyAttributeError>(py) => {}
                Err(err) => return Err(err),
            }
            Ok::<(), PyErr>(())
        })
        .with_context(|| format!("Python strategy callback {method} failed"))
    }

    fn record_runtime_error(&mut self, error: anyhow::Error) {
        let message = format!("{error:#}");
        self.runtime_error = Some(message);
        self.state.lock().unwrap().status = AlgorithmStatus::RuntimeError;
    }
}

pub fn load_strategy_bridge_with_context(
    strategy_path: &Path,
    context: AlgorithmConstructionContext,
) -> Result<PythonAlgorithmBridge> {
    Python::attach(|py| {
        let state = context.state();
        let runtime_services = context.runtime_services();
        let instance = AlgorithmHandle::with_default_context(context, || {
            strategy_loader::load_strategy_file(py, strategy_path)
        })?
        .instance;
        Ok(PythonAlgorithmBridge::from_strategy(
            py,
            instance,
            state,
            runtime_services,
        )?)
    })
}

impl AlgorithmStateAccess for PythonAlgorithmBridge {
    fn algorithm_state(&self) -> Option<Arc<Mutex<QcAlgorithm>>> {
        Some(self.state.clone())
    }
}

impl LifecycleBridge for PythonAlgorithmBridge {
    fn initialize(&mut self, _services: &mut dyn AlgorithmServices) -> Result<()> {
        debug_log_sigint_state(
            "pre-fix",
            "H5",
            "crates/lean-python-runtime/src/bridge.rs:before_strategy_initialize",
        );
        let result = self.call_method0_if_present_any(&["initialize", "Initialize"]);
        debug_log_sigint_state(
            "pre-fix",
            "H5",
            "crates/lean-python-runtime/src/bridge.rs:after_strategy_initialize",
        );
        result
    }

    fn on_data(&mut self, payload: DataDeliveryPayload, _services: &mut dyn AlgorithmServices) {
        if let Err(error) = self.call_on_data(payload) {
            self.record_runtime_error(error);
        }
    }

    fn on_order_event(&mut self, event: &OrderEvent, _services: &mut dyn AlgorithmServices) {
        if let Err(error) = self.call_order_event_callback(
            &["on_order_event", "OnOrderEvent"],
            OrderEventView::new(event.clone()),
        ) {
            self.record_runtime_error(error);
        }
    }

    fn on_otm_expiry(
        &mut self,
        contract: OptionContract,
        quantity: Decimal,
        underlying_price: Decimal,
        entry_premium: Decimal,
        _services: &mut dyn AlgorithmServices,
    ) {
        let event = otm_expiry_event_view(
            contract.symbol,
            self.state.lock().unwrap().utc_time.0,
            quantity,
            underlying_price,
            entry_premium,
        );
        if let Err(error) =
            self.call_order_event_callback(&["on_order_event", "OnOrderEvent"], event)
        {
            self.record_runtime_error(error);
        }
    }

    fn on_assignment_order_event(
        &mut self,
        contract: OptionContract,
        quantity: Decimal,
        is_assignment: bool,
        _services: &mut dyn AlgorithmServices,
    ) {
        let event = assignment_expiry_event_view(
            contract.symbol,
            self.state.lock().unwrap().utc_time.0,
            quantity,
            is_assignment,
        );
        if let Err(error) = self.call_order_event_callback(
            &["on_assignment_order_event", "OnAssignmentOrderEvent"],
            event,
        ) {
            self.record_runtime_error(error);
        }
    }

    fn on_end_of_day(&mut self, symbol: Option<Symbol>, _services: &mut dyn AlgorithmServices) {
        if let Some(symbol) = symbol {
            let result = Python::attach(|py| {
                Py::new(py, SymbolHandle::new(symbol)).map(|value| value.into_any())
            })
            .context("failed to create end-of-day symbol payload")
            .and_then(|payload| self.call_method1_if_present("on_end_of_day", payload));
            if let Err(error) = result {
                self.record_runtime_error(error);
            }
            return;
        }
        let _ = self.call_method0_if_present("on_end_of_day");
    }

    fn on_warmup_finished(&mut self, _services: &mut dyn AlgorithmServices) {
        let _ = self.call_method0_if_present("on_warmup_finished");
    }

    fn on_end_of_algorithm(&mut self, _services: &mut dyn AlgorithmServices) {
        let _ = self.call_method0_if_present("on_end_of_algorithm");
    }

    fn on_margin_call(&mut self, requests: &[Order], _services: &mut dyn AlgorithmServices) {
        let margin_requests = margin_call_requests_from_orders(requests);
        let payload = margin_call_payload(&margin_requests);
        let result = Python::attach(|py| {
            let py_requests = pyo3::types::PyList::empty(py);
            for (symbol, quantity) in payload {
                let item = (symbol, quantity);
                py_requests.append(item)?;
            }
            Ok::<_, PyErr>(py_requests.into_any().unbind())
        })
        .context("failed to create margin call callback payload")
        .and_then(|payload| self.call_method1_if_present("on_margin_call", payload));
        if let Err(error) = result {
            self.record_runtime_error(error);
        }
    }

    fn on_margin_call_warning(&mut self, _services: &mut dyn AlgorithmServices) {
        let _ = self.call_method0_if_present("on_margin_call_warning");
    }

    fn on_securities_changed(
        &mut self,
        changes: &SecurityChanges,
        _services: &mut dyn AlgorithmServices,
    ) {
        let event =
            SecurityChangesView::from_symbols(changes.added.clone(), changes.removed.clone());
        let result = Python::attach(|py| Py::new(py, event).map(|value| value.into_any()))
            .context("failed to create SecurityChangesView")
            .and_then(|event| self.call_method1_if_present("on_securities_changed", event));
        if let Err(error) = result {
            self.record_runtime_error(error);
        }
    }

    fn on_splits(&mut self, splits: &HashMap<u64, Split>, _services: &mut dyn AlgorithmServices) {
        let result = Python::attach(|py| {
            let event_dict = PyDict::new(py);
            for sid in splits.keys() {
                event_dict.set_item(*sid, *sid)?;
            }
            Ok::<_, PyErr>(event_dict.into_any().unbind())
        })
        .context("failed to create splits callback payload")
        .and_then(|payload| self.call_method1_if_present("on_splits", payload));
        if let Err(error) = result {
            self.record_runtime_error(error);
        }
    }

    fn on_dividends(
        &mut self,
        dividends: &HashMap<u64, Dividend>,
        _services: &mut dyn AlgorithmServices,
    ) {
        let result = Python::attach(|py| {
            let event_dict = PyDict::new(py);
            for sid in dividends.keys() {
                event_dict.set_item(*sid, *sid)?;
            }
            Ok::<_, PyErr>(event_dict.into_any().unbind())
        })
        .context("failed to create dividends callback payload")
        .and_then(|payload| self.call_method1_if_present("on_dividends", payload));
        if let Err(error) = result {
            self.record_runtime_error(error);
        }
    }

    fn on_delistings(
        &mut self,
        delistings: &HashMap<u64, Delisting>,
        _services: &mut dyn AlgorithmServices,
    ) {
        let result = Python::attach(|py| {
            let event_dict = PyDict::new(py);
            for sid in delistings.keys() {
                event_dict.set_item(*sid, *sid)?;
            }
            Ok::<_, PyErr>(event_dict.into_any().unbind())
        })
        .context("failed to create delistings callback payload")
        .and_then(|payload| self.call_method1_if_present("on_delistings", payload));
        if let Err(error) = result {
            self.record_runtime_error(error);
        }
    }

    fn on_symbol_changed_events(
        &mut self,
        events: &HashMap<u64, SymbolChangedEvent>,
        _services: &mut dyn AlgorithmServices,
    ) {
        let result = Python::attach(|py| {
            let event_dict = PyDict::new(py);
            for sid in events.keys() {
                event_dict.set_item(*sid, *sid)?;
            }
            Ok::<_, PyErr>(event_dict.into_any().unbind())
        })
        .context("failed to create symbol change callback payload")
        .and_then(|payload| self.call_method1_if_present("on_symbol_changed_events", payload));
        if let Err(error) = result {
            self.record_runtime_error(error);
        }
    }

    fn select_universe_changes(
        &mut self,
        _utc_ns: i64,
        _resolution: Resolution,
        _services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        Vec::new()
    }

    fn select_custom_universe_changes(
        &mut self,
        utc_ns: i64,
        resolution: Resolution,
        custom_data: &HashMap<String, Vec<CustomDataPoint>>,
        services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        let Some(runtime) = services.runtime_services() else {
            return self.runtime_services.run_custom_universe_selections(
                utc_ns,
                resolution,
                custom_data,
            );
        };
        runtime.run_custom_universe_selections(utc_ns, resolution, custom_data)
    }

    fn on_end_of_time_step(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_message(&mut self, _message: &str, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_disconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_reconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn terminal_status(&self) -> Option<AlgorithmStatus> {
        self.runtime_error
            .as_ref()
            .map(|_| AlgorithmStatus::RuntimeError)
    }

    fn runtime_error(&self) -> Option<String> {
        self.runtime_error.clone()
    }

    fn name(&self) -> &str {
        "PythonAlgorithm"
    }

    fn start_date(&self) -> DateTime {
        self.state.lock().unwrap().start_date
    }

    fn end_date(&self) -> DateTime {
        self.state.lock().unwrap().end_date
    }

    fn portfolio_value(&self) -> Price {
        self.state.lock().unwrap().portfolio_value()
    }

    fn starting_cash(&self) -> Price {
        self.state.lock().unwrap().portfolio.starting_cash()
    }

    fn subscriptions(&self) -> Vec<SubscriptionDataConfig> {
        self.state
            .lock()
            .unwrap()
            .subscription_manager
            .get_all()
            .into_iter()
            .map(|config| (*config).clone())
            .collect()
    }

    fn prepare_data_delivery(&mut self, _subscriptions: &[SubscriptionDataConfig]) -> Result<()> {
        Ok(())
    }

    fn option_subscriptions(&self) -> Vec<OptionSubscription> {
        let algorithm = self.state.lock().unwrap();
        algorithm
            .option_subscriptions
            .iter()
            .cloned()
            .map(|canonical| {
                let resolution = algorithm
                    .option_subscription_resolutions
                    .get(canonical.permtick.as_ref())
                    .copied()
                    .unwrap_or(Resolution::Daily);
                let filter = algorithm
                    .option_filters
                    .get(canonical.permtick.as_ref())
                    .copied()
                    .unwrap_or_default();
                OptionSubscription {
                    canonical,
                    resolution,
                    filter,
                }
            })
            .collect()
    }

    fn portfolio(&self) -> Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>> {
        Some(self.state.lock().unwrap().portfolio.clone())
    }

    fn transactions(&self) -> Option<Arc<TransactionManager>> {
        Some(self.state.lock().unwrap().transactions.clone())
    }

    fn order_fee(&self, order: &Order, fill_price: Price) -> Price {
        self.state
            .lock()
            .unwrap()
            .order_fee(order, fill_price)
            .amount
    }

    fn contract_multiplier_for_symbol(&self, symbol: &Symbol) -> Price {
        self.state
            .lock()
            .unwrap()
            .contract_multiplier_for_symbol(symbol)
    }

    fn validate_order_buying_power(
        &self,
        order: &Order,
        fill_price: Price,
        fee: Price,
    ) -> std::result::Result<(), String> {
        self.state
            .lock()
            .unwrap()
            .validate_order_buying_power(order, fill_price, fee)
    }

    fn has_universes(&self) -> bool {
        self.runtime_services.has_custom_universe_selectors()
    }

    fn universe_resolution(&self) -> Option<Resolution> {
        self.runtime_services.custom_universe_selector_resolution()
    }

    fn alpha_analytics(&self) -> AlphaAnalytics {
        AlphaAnalytics::default()
    }

    fn charts(&self) -> ChartCollection {
        ChartCollection::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AlgorithmImports;
    use lean_core::TimeSpan;
    use lean_data::trade_bar::TradeBarData;
    use lean_data::Slice;
    use lean_data::TradeBar;
    use rust_decimal_macros::dec;
    use std::fs;
    use std::sync::Once;

    fn init_python() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            pyo3::append_to_inittab!(AlgorithmImports);
            pyo3::Python::initialize();
        });
    }

    fn load_test_strategy_bridge(path: &Path) -> PythonAlgorithmBridge {
        let state = Arc::new(Mutex::new(QcAlgorithm::new("Algorithm", dec!(100000))));
        let runtime_context = lean_engine::AlgorithmRuntimeContext::with_history_service(
            Arc::new(lean_algorithm::lifecycle::NullHistoryService),
            HashMap::new(),
        );
        let context = AlgorithmConstructionContext::new_with_runtime_services(
            state,
            Arc::new(runtime_context),
        );
        load_strategy_bridge_with_context(path, context).unwrap()
    }

    #[test]
    fn python_main_py_backtest_bridge_can_startup() {
        init_python();

        let dir = std::env::temp_dir().join(format!("rlean-python-startup-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

class StartupAlgorithm(QCAlgorithm):
    def initialize(self):
        self.set_cash(12345.0)
        self.spy = self.add_equity("SPY", Resolution.Daily).symbol
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();

        bridge.initialize(&mut services).unwrap();

        assert_eq!(bridge.name(), "PythonAlgorithm");
        assert_eq!(bridge.starting_cash(), dec!(12345));
        assert_eq!(bridge.subscriptions().len(), 1);
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn python_bridge_exposes_option_subscriptions_after_add_option() {
        init_python();

        let dir = std::env::temp_dir().join(format!(
            "rlean-python-option-subscriptions-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

class OptionAlgorithm(QCAlgorithm):
    def initialize(self):
        option = self.add_option("SPY", Resolution.Daily)
        option.set_filter(-15, 15, 0, 90)
        self.canon = option.symbol
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        let option_subscriptions = bridge.option_subscriptions();
        assert_eq!(option_subscriptions.len(), 1);
        let subscription = &option_subscriptions[0];
        assert_eq!(subscription.canonical.permtick.as_ref(), "?SPY");
        assert_eq!(subscription.resolution, Resolution::Daily);
        assert_eq!(subscription.filter.min_strike_rank, -15);
        assert_eq!(subscription.filter.max_strike_rank, 15);
        assert_eq!(subscription.filter.min_expiry_days, 0);
        assert_eq!(subscription.filter.max_expiry_days, 90);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn python_add_universe_registers_custom_selector() {
        init_python();

        let dir = std::env::temp_dir().join(format!(
            "rlean-python-custom-universe-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

class UniverseAlgorithm(QCAlgorithm):
    def initialize(self):
        self.universe_settings.resolution = Resolution.Minute
        self.add_universe("fixture", "snapshot", Resolution.Daily, self.select)

    def select(self, points):
        return ["SPY"]
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        assert!(bridge.has_universes());
        assert_eq!(bridge.universe_resolution(), Some(Resolution::Daily));
        assert_eq!(bridge.subscriptions().len(), 1);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn python_custom_universe_selector_returns_security_changes() {
        init_python();

        let dir = std::env::temp_dir().join(format!(
            "rlean-python-custom-universe-select-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

class UniverseAlgorithm(QCAlgorithm):
    def initialize(self):
        self.universe_settings.resolution = Resolution.Minute
        self.add_universe("tradealert", "snapshot", Resolution.Daily, self.select)

    def select(self, points):
        return [str(point.fields["usymbol"]) for point in points]
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        let mut fields = HashMap::new();
        fields.insert(
            "usymbol".to_string(),
            serde_json::Value::String("SPY".to_string()),
        );
        let point = CustomDataPoint {
            time: chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            end_time: Some(lean_core::DateTime::from(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                    .unwrap()
                    .and_hms_opt(16, 0, 0)
                    .unwrap(),
            )),
            value: dec!(1),
            symbol: None,
            fields: Arc::new(fields),
        };
        let changes = bridge.select_custom_universe_changes(
            point.end_time.unwrap().0,
            Resolution::Daily,
            &HashMap::from([("SNAPSHOT".to_string(), vec![point])]),
            &mut services,
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].changes.added.len(), 1);
        assert_eq!(changes[0].changes.added[0].value.as_ref(), "SPY");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn python_custom_fields_view_is_dict_compatible() {
        init_python();

        let dir = std::env::temp_dir().join(format!(
            "rlean-python-custom-fields-view-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

class UniverseAlgorithm(QCAlgorithm):
    def initialize(self):
        self.add_universe("tradealert", "snapshot", Resolution.Daily, self.select)

    def select(self, points):
        out = []
        for point in points:
            # get() with a non-string default returns that object (dict-compat).
            assert point.fields.get("missing", 0) == 0
            assert point.fields.get("missing") is None
            assert point.fields.get("usymbol") == "SPY"
            # values() returns every field's string value.
            values = sorted(point.fields.values())
            assert "SPY" in values
            # Canonical symbol is exposed on the point itself.
            assert point.symbol == "SPY"
            out.append(point.symbol)
        return out
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        let mut fields = HashMap::new();
        fields.insert(
            "usymbol".to_string(),
            serde_json::Value::String("SPY".to_string()),
        );
        fields.insert("score".to_string(), serde_json::json!(1.25));
        let point = CustomDataPoint {
            time: chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap(),
            end_time: Some(lean_core::DateTime::from(
                chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                    .unwrap()
                    .and_hms_opt(16, 0, 0)
                    .unwrap(),
            )),
            value: dec!(1),
            symbol: Some("SPY".to_string()),
            fields: Arc::new(fields),
        };
        let changes = bridge.select_custom_universe_changes(
            point.end_time.unwrap().0,
            Resolution::Daily,
            &HashMap::from([("SNAPSHOT".to_string(), vec![point])]),
            &mut services,
        );

        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].changes.added.len(), 1);
        assert_eq!(changes[0].changes.added[0].value.as_ref(), "SPY");

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn python_on_data_receives_generated_slice_view() {
        init_python();

        let dir = std::env::temp_dir().join(format!("rlean-python-on-data-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution, SimpleMovingAverage

class DataAlgorithm(QCAlgorithm):
    def initialize(self):
        self.spy = self.add_equity("SPY", Resolution.Daily).symbol
        self.sma = SimpleMovingAverage(1)

    def on_data(self, data):
        self.seen_has_data = data.has_data
        bar = data.bars.get(self.spy)
        self.seen_close = None if bar is None else bar.close
        self.seen_end_time = None if bar is None else bar.end_time
        if bar is not None:
            self.sma.update(bar.end_time, bar.close)
            self.seen_sma = self.sma.current.value
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        let symbol = bridge.subscriptions()[0].symbol.clone();
        let time = DateTime::from_secs(1_700_000_000);
        let mut slice = Slice::new(time);
        slice.add_bar(TradeBar::new(
            symbol,
            time,
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(105), dec!(99), dec!(102.5), dec!(1000)),
        ));

        bridge.on_data(
            DataDeliveryPayload {
                slice: Arc::new(slice),
            },
            &mut services,
        );

        assert_eq!(bridge.runtime_error(), None);
        Python::attach(|py| {
            let strategy = bridge.strategy.bind(py);
            assert!(strategy
                .getattr("seen_has_data")
                .unwrap()
                .extract::<bool>()
                .unwrap());
            assert_eq!(
                strategy
                    .getattr("seen_close")
                    .unwrap()
                    .extract::<f64>()
                    .unwrap(),
                102.5
            );
            assert!(strategy
                .getattr("seen_end_time")
                .unwrap()
                .hasattr("year")
                .unwrap());
            assert_eq!(
                strategy
                    .getattr("seen_sma")
                    .unwrap()
                    .extract::<f64>()
                    .unwrap(),
                102.5
            );
        });

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn python_on_data_can_iterate_delivered_option_chain() {
        init_python();

        let dir = std::env::temp_dir().join(format!(
            "rlean-python-option-chain-iteration-{}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.py");
        fs::write(
            &path,
            r#"
from AlgorithmImports import QCAlgorithm, Resolution

class OptionChainAlgorithm(QCAlgorithm):
    def initialize(self):
        option = self.add_option("SPY", Resolution.Daily)
        self.canon = option.symbol

    def on_data(self, data):
        chain = data.option_chains.get(self.canon)
        self.chain_count = -1 if chain is None else sum(1 for c in chain if c.symbol is not None)
"#,
        )
        .unwrap();

        let mut bridge = load_test_strategy_bridge(&path);
        let mut services = lean_algorithm::lifecycle::NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        let subscription = bridge.option_subscriptions()[0].clone();
        let time = DateTime::from_secs(1_700_000_000);
        let mut slice = Slice::new(time);
        let underlying = subscription
            .canonical
            .underlying
            .as_ref()
            .unwrap()
            .as_ref()
            .clone();
        slice.add_bar(TradeBar::new(
            underlying.clone(),
            time,
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(105), dec!(99), dec!(102.5), dec!(1000)),
        ));

        let option_symbol = Symbol::create_option(
            underlying,
            &lean_core::Market::usa(),
            time.date_utc().succ_opt().unwrap(),
            dec!(100),
            lean_core::OptionRight::Put,
            lean_core::OptionStyle::American,
        );
        let mut chain = lean_options::OptionChain::new(subscription.canonical.clone(), dec!(102.5));
        chain.add_contract(lean_options::OptionContract::new(option_symbol));
        slice.add_option_chain(subscription.canonical.permtick.to_string(), Arc::new(chain));

        bridge.on_data(
            DataDeliveryPayload {
                slice: Arc::new(slice),
            },
            &mut services,
        );

        assert_eq!(bridge.runtime_error(), None);
        Python::attach(|py| {
            let strategy = bridge.strategy.bind(py);
            assert_eq!(
                strategy
                    .getattr("chain_count")
                    .unwrap()
                    .extract::<i64>()
                    .unwrap(),
                1
            );
        });

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }
}
