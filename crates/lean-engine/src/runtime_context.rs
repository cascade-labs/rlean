use crate::custom_universe::{
    custom_universe_resolution, has_custom_universe_selectors, register_custom_universe_selector,
    run_custom_universe_selections, CustomUniverseSelectorRegistry,
};
use crate::engine_framework_registry::EngineFrameworkRegistry;
use crate::framework::FrameworkState;
use crate::history_service::{AlgorithmHistoryContext, HistoryService};
use lean_algorithm::lifecycle::{
    AlgorithmHistoryService, AlgorithmRuntimeServices, AlgorithmServices,
    CustomUniverseSelectorRegistrationRequest as RegistrationRequest, RegisteredIndicatorRegistry,
};
use lean_algorithm::FrameworkModelRegistry;
use lean_core::{DateTime, Resolution};
use lean_data_providers::{ICustomDataSource, IHistoryProvider};
use lean_storage::IcebergStore;
use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

#[derive(Clone)]
pub struct AlgorithmRuntimeContext {
    history_service: Arc<dyn AlgorithmHistoryService>,
    runtime_parameters: Arc<RwLock<HashMap<String, String>>>,
    registered_indicators: RegisteredIndicatorRegistry,
    framework: Arc<Mutex<FrameworkState>>,
    framework_registry: Arc<EngineFrameworkRegistry>,
    custom_universe_selectors: CustomUniverseSelectorRegistry,
}

impl AlgorithmRuntimeContext {
    pub fn new(
        data_root: PathBuf,
        data_store: Arc<IcebergStore>,
        history_provider: Option<Arc<dyn IHistoryProvider>>,
        custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
        runtime_parameters: HashMap<String, String>,
    ) -> Self {
        let history_service = Arc::new(HistoryService::new(AlgorithmHistoryContext {
            data_root,
            data_store,
            history_provider,
            custom_data_sources,
        }));
        Self::with_history_service(history_service, runtime_parameters)
    }

    pub fn with_history_service(
        history_service: Arc<dyn AlgorithmHistoryService>,
        runtime_parameters: HashMap<String, String>,
    ) -> Self {
        let framework = Arc::new(Mutex::new(FrameworkState::default()));
        Self {
            history_service,
            runtime_parameters: Arc::new(RwLock::new(runtime_parameters)),
            registered_indicators: Arc::new(Mutex::new(HashMap::new())),
            framework: framework.clone(),
            framework_registry: Arc::new(EngineFrameworkRegistry::new(framework)),
            custom_universe_selectors: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn history_service(&self) -> Arc<dyn AlgorithmHistoryService> {
        self.history_service.clone()
    }

    pub fn runtime_parameters(&self) -> Arc<RwLock<HashMap<String, String>>> {
        self.runtime_parameters.clone()
    }

    pub fn registered_indicators(&self) -> RegisteredIndicatorRegistry {
        self.registered_indicators.clone()
    }

    pub fn framework(&self) -> Arc<Mutex<FrameworkState>> {
        self.framework.clone()
    }

    pub fn framework_registry(&self) -> Arc<dyn FrameworkModelRegistry> {
        self.framework_registry.clone()
    }

    pub fn custom_universe_selectors(&self) -> CustomUniverseSelectorRegistry {
        self.custom_universe_selectors.clone()
    }

    pub fn with_framework(mut self, framework: Arc<Mutex<FrameworkState>>) -> Self {
        self.framework = framework.clone();
        self.framework_registry = Arc::new(EngineFrameworkRegistry::new(framework));
        self
    }

    pub fn update_registered_indicators(&self, slice: &lean_data::Slice) {
        let registry = self
            .registered_indicators
            .lock()
            .expect("registered indicator registry poisoned");
        for (sid, indicators) in registry.iter() {
            let Some(bar) = slice.bars.get(sid) else {
                continue;
            };
            for indicator in indicators {
                indicator.update_bar(bar);
            }
        }
    }
}

pub struct EngineAlgorithmServices {
    time: DateTime,
    context: AlgorithmRuntimeContext,
}

impl EngineAlgorithmServices {
    pub fn new(time: DateTime, context: AlgorithmRuntimeContext) -> Self {
        Self { time, context }
    }

    pub fn context(&self) -> &AlgorithmRuntimeContext {
        &self.context
    }

    pub fn set_time(&mut self, time: DateTime) {
        self.time = time;
    }
}

impl AlgorithmServices for EngineAlgorithmServices {
    fn time(&self) -> DateTime {
        self.time
    }

    fn set_runtime_parameter(&mut self, key: &str, value: String) {
        self.context
            .runtime_parameters
            .write()
            .expect("runtime parameter lock poisoned")
            .insert(key.to_string(), value);
    }

    fn runtime_parameter(&self, key: &str) -> Option<String> {
        self.context
            .runtime_parameters
            .read()
            .expect("runtime parameter lock poisoned")
            .get(key)
            .cloned()
    }

    fn emit_debug(&mut self, message: &str) {
        tracing::debug!("{message}");
    }

    fn history_service(&self) -> Arc<dyn AlgorithmHistoryService> {
        self.context.history_service()
    }

    fn runtime_services(&self) -> Option<Arc<dyn AlgorithmRuntimeServices>> {
        Some(Arc::new(self.context.clone()))
    }
}

impl AlgorithmRuntimeServices for AlgorithmRuntimeContext {
    fn history_service(&self) -> Arc<dyn AlgorithmHistoryService> {
        self.history_service()
    }

    fn runtime_parameters(&self) -> Arc<RwLock<HashMap<String, String>>> {
        self.runtime_parameters()
    }

    fn registered_indicators(&self) -> RegisteredIndicatorRegistry {
        self.registered_indicators()
    }

    fn framework_state(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.framework())
    }

    fn framework_registry(&self) -> Option<Arc<dyn FrameworkModelRegistry>> {
        Some(self.framework_registry())
    }

    fn custom_universe_registry(&self) -> Option<Arc<dyn Any + Send + Sync>> {
        Some(self.custom_universe_selectors())
    }

    fn register_custom_universe_selector(&self, registration: RegistrationRequest) {
        let settings = lean_sdk::universe::UniverseSettings {
            resolution: registration.settings_resolution,
            minimum_time_in_universe_secs: registration.minimum_time_in_universe_secs,
            ..lean_sdk::universe::UniverseSettings::default()
        };
        let descriptor = lean_sdk::universe::ScheduledUniverseDescriptor::custom_data(
            registration.source_type,
            registration.ticker.clone(),
            registration.resolution,
            settings,
            lean_core::SecurityType::Equity,
            lean_core::Market::usa(),
        );
        register_custom_universe_selector(
            &self.custom_universe_selectors,
            registration.ticker.to_ascii_uppercase(),
            registration.resolution,
            descriptor,
            registration.select,
        );
    }

    fn run_custom_universe_selections(
        &self,
        utc_ns: i64,
        resolution: Resolution,
        custom_data: &HashMap<String, Vec<lean_data::CustomDataPoint>>,
    ) -> Vec<lean_algorithm::lifecycle::UniverseSelection> {
        run_custom_universe_selections(
            &self.custom_universe_selectors,
            utc_ns,
            resolution,
            custom_data,
        )
    }

    fn has_custom_universe_selectors(&self) -> bool {
        has_custom_universe_selectors(&self.custom_universe_selectors)
    }

    fn custom_universe_selector_resolution(&self) -> Option<Resolution> {
        custom_universe_resolution(&self.custom_universe_selectors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_algorithm::lifecycle::HistoryColumns;
    use lean_algorithm::qc_algorithm::QcAlgorithm;
    use lean_core::{Market, Resolution, Symbol, TimeSpan};
    use lean_data::{TradeBar, TradeBarData};
    use rust_decimal_macros::dec;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHistoryService;

    impl AlgorithmHistoryService for CountingHistoryService {
        fn history(
            &self,
            _algorithm: &QcAlgorithm,
            _symbol: &Symbol,
            _periods: usize,
            _resolution: Resolution,
        ) -> HistoryColumns {
            HashMap::from([("close".to_string(), vec!["1.0".to_string()])])
        }
    }

    struct CountingIndicator {
        updates: Arc<AtomicUsize>,
    }

    impl lean_algorithm::lifecycle::RegisteredIndicatorBridge for CountingIndicator {
        fn update_bar(&self, _bar: &TradeBar) -> bool {
            self.updates.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[test]
    fn services_share_history_and_runtime_parameters() {
        let context = AlgorithmRuntimeContext::with_history_service(
            Arc::new(CountingHistoryService),
            HashMap::from([("existing".to_string(), "value".to_string())]),
        );
        let mut services =
            EngineAlgorithmServices::new(DateTime::from_secs(1_700_000_000), context.clone());

        assert_eq!(
            services.runtime_parameter("existing"),
            Some("value".to_string())
        );
        services.set_runtime_parameter("new", "parameter".to_string());
        assert_eq!(
            context
                .runtime_parameters()
                .read()
                .unwrap()
                .get("new")
                .cloned(),
            Some("parameter".to_string())
        );

        let algorithm = QcAlgorithm::new("test", dec!(100000));
        let history = services.history_service().history(
            &algorithm,
            &Symbol::create_equity("SPY", &Market::usa()),
            1,
            Resolution::Daily,
        );
        assert_eq!(history.get("close"), Some(&vec!["1.0".to_string()]));
    }

    #[test]
    fn runtime_context_advances_registered_indicators() {
        let context = AlgorithmRuntimeContext::with_history_service(
            Arc::new(CountingHistoryService),
            HashMap::new(),
        );
        let updates = Arc::new(AtomicUsize::new(0));
        context
            .registered_indicators()
            .lock()
            .unwrap()
            .entry(1)
            .or_default()
            .push(Arc::new(CountingIndicator {
                updates: updates.clone(),
            }));

        let mut slice = lean_data::Slice::new(DateTime::from_secs(1_700_000_000));
        slice.bars.insert(
            1,
            TradeBar::new(
                Symbol::create_equity("SPY", &Market::usa()),
                DateTime::from_secs(1_700_000_000),
                TimeSpan::from_days(1),
                TradeBarData {
                    open: dec!(1),
                    high: dec!(1),
                    low: dec!(1),
                    close: dec!(1),
                    volume: dec!(1),
                },
            ),
        );

        context.update_registered_indicators(&slice);
        assert_eq!(updates.load(Ordering::SeqCst), 1);
    }
}
