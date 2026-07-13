use rlean_core::{DataNormalizationMode, Market, Resolution, SecurityType, Symbol};
use rlean_data::LiveUniverseSubscriptionConfig;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct UniverseSettings {
    pub resolution: Resolution,
    pub leverage: f64,
    pub fill_forward: bool,
    pub extended_market_hours: bool,
    pub minimum_time_in_universe_secs: f64,
    pub data_normalization_mode: DataNormalizationMode,
}

impl Default for UniverseSettings {
    fn default() -> Self {
        Self {
            resolution: Resolution::Daily,
            leverage: 1.0,
            fill_forward: true,
            extended_market_hours: false,
            minimum_time_in_universe_secs: 0.0,
            data_normalization_mode: DataNormalizationMode::Adjusted,
        }
    }
}

impl UniverseSettings {
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "UniverseSettings"))]
pub struct UniverseSettingsHandle {
    inner: std::sync::Arc<std::sync::Mutex<UniverseSettings>>,
}

impl UniverseSettingsHandle {
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(UniverseSettings::default())),
        }
    }

    pub fn from_shared(inner: std::sync::Arc<std::sync::Mutex<UniverseSettings>>) -> Self {
        Self { inner }
    }

    pub fn snapshot(&self) -> UniverseSettings {
        self.inner.lock().unwrap().clone()
    }
    pub fn resolution(&self) -> Resolution {
        self.inner.lock().unwrap().resolution
    }

    pub fn set_resolution(&self, resolution: Resolution) {
        self.inner.lock().unwrap().resolution = resolution;
    }
    pub fn leverage(&self) -> f64 {
        self.inner.lock().unwrap().leverage
    }

    pub fn set_leverage(&self, leverage: f64) {
        self.inner.lock().unwrap().leverage = leverage;
    }
    pub fn fill_forward(&self) -> bool {
        self.inner.lock().unwrap().fill_forward
    }

    pub fn set_fill_forward(&self, fill_forward: bool) {
        self.inner.lock().unwrap().fill_forward = fill_forward;
    }
    pub fn extended_market_hours(&self) -> bool {
        self.inner.lock().unwrap().extended_market_hours
    }

    pub fn set_extended_market_hours(&self, extended_market_hours: bool) {
        self.inner.lock().unwrap().extended_market_hours = extended_market_hours;
    }
    pub fn minimum_time_in_universe(&self) -> f64 {
        self.inner.lock().unwrap().minimum_time_in_universe_secs
    }

    pub fn set_minimum_time_in_universe(&self, seconds: f64) {
        self.inner.lock().unwrap().minimum_time_in_universe_secs = seconds.max(0.0);
    }
    pub fn data_normalization_mode(&self) -> DataNormalizationMode {
        self.inner.lock().unwrap().data_normalization_mode
    }

    pub fn set_data_normalization_mode(&self, mode: DataNormalizationMode) {
        self.inner.lock().unwrap().data_normalization_mode = mode;
    }
}

impl Default for UniverseSettingsHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl UniverseSettingsHandle {
    #[new]
    fn py_new() -> Self {
        Self::new()
    }

    #[getter(resolution)]
    fn py_resolution(&self) -> crate::types::Resolution {
        self.resolution().into()
    }

    #[setter(resolution)]
    fn py_set_resolution(&self, resolution: crate::types::Resolution) {
        self.set_resolution(resolution.into());
    }

    #[pyo3(name = "set_resolution")]
    fn py_set_resolution_method(&self, resolution: crate::types::Resolution) {
        self.set_resolution(resolution.into());
    }

    #[getter(leverage)]
    fn py_leverage(&self) -> f64 {
        self.leverage()
    }

    #[setter(leverage)]
    fn py_set_leverage(&self, leverage: f64) {
        self.set_leverage(leverage);
    }

    #[getter(fill_forward)]
    fn py_fill_forward(&self) -> bool {
        self.fill_forward()
    }

    #[setter(fill_forward)]
    fn py_set_fill_forward(&self, fill_forward: bool) {
        self.set_fill_forward(fill_forward);
    }

    #[getter(extended_market_hours)]
    fn py_extended_market_hours(&self) -> bool {
        self.extended_market_hours()
    }

    #[setter(extended_market_hours)]
    fn py_set_extended_market_hours(&self, extended_market_hours: bool) {
        self.set_extended_market_hours(extended_market_hours);
    }

    #[getter(minimum_time_in_universe)]
    fn py_minimum_time_in_universe(&self) -> f64 {
        self.minimum_time_in_universe()
    }

    #[setter(minimum_time_in_universe)]
    fn py_set_minimum_time_in_universe(&self, seconds: f64) {
        self.set_minimum_time_in_universe(seconds);
    }

    #[getter(data_normalization_mode)]
    fn py_data_normalization_mode(&self) -> crate::types::DataNormalizationMode {
        self.data_normalization_mode().into()
    }

    #[setter(data_normalization_mode)]
    fn py_set_data_normalization_mode(&self, mode: crate::types::DataNormalizationMode) {
        self.set_data_normalization_mode(mode.into());
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateRule {
    EveryDay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRule {
    At { hour: u32, minute: u32 },
    AfterMarketOpen { minutes_after_open: i64 },
    EveryResolution,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "DateRule"))]
pub struct DateRuleHandle {
    pub kind: DateRule,
}

impl DateRuleHandle {
    pub fn new(kind: DateRule) -> Self {
        Self { kind }
    }
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "DateRules"))]
pub struct DateRulesHandle;

impl DateRulesHandle {
    pub fn new() -> Self {
        Self
    }

    pub fn every_day(&self) -> DateRuleHandle {
        DateRuleHandle::new(DateRule::EveryDay)
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "TimeRule"))]
pub struct TimeRuleHandle {
    pub kind: TimeRule,
}

impl TimeRuleHandle {
    pub fn new(kind: TimeRule) -> Self {
        Self { kind }
    }
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "TimeRules"))]
pub struct TimeRulesHandle;

impl TimeRulesHandle {
    pub fn new() -> Self {
        Self
    }

    pub fn at(&self, hour: u32, minute: u32, _second: u32) -> TimeRuleHandle {
        TimeRuleHandle::new(TimeRule::At { hour, minute })
    }

    pub fn after_market_open(&self, minutes_after_open: i64) -> TimeRuleHandle {
        TimeRuleHandle::new(TimeRule::AfterMarketOpen { minutes_after_open })
    }

    pub fn every(&self, interval_seconds: i64) -> TimeRuleHandle {
        let minutes_after_open = (interval_seconds / 60).max(0);
        TimeRuleHandle::new(TimeRule::AfterMarketOpen { minutes_after_open })
    }
}

#[derive(Debug, Clone)]
pub struct ScheduledUniverseDescriptor {
    pub date_rule: DateRule,
    pub time_rule: TimeRule,
    pub settings: UniverseSettings,
    pub trigger_resolution: Resolution,
    pub symbol_security_type: SecurityType,
    pub symbol_market: Market,
    pub custom_source_type: Option<String>,
    pub custom_ticker: Option<String>,
    pub custom_properties: HashMap<String, String>,
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "ScheduledUniverse"))]
pub struct ScheduledUniverseHandle {
    descriptor: ScheduledUniverseDescriptor,
}

impl ScheduledUniverseHandle {
    pub fn new(
        date_rule: DateRuleHandle,
        time_rule: TimeRuleHandle,
        settings: Option<UniverseSettings>,
    ) -> Self {
        Self {
            descriptor: ScheduledUniverseDescriptor::scheduled(
                date_rule.kind,
                time_rule.kind,
                settings.unwrap_or_default(),
            ),
        }
    }

    pub fn from_descriptor(descriptor: ScheduledUniverseDescriptor) -> Self {
        Self { descriptor }
    }

    pub fn descriptor(&self) -> &ScheduledUniverseDescriptor {
        &self.descriptor
    }
    pub fn settings(&self) -> UniverseSettings {
        self.descriptor.settings.clone()
    }

    pub fn with_custom_properties(&self, properties: HashMap<String, String>) -> Self {
        Self {
            descriptor: self.descriptor.clone().with_custom_properties(properties),
        }
    }
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "SecurityChanges"))]
pub struct SecurityChangesView {
    added: Vec<Symbol>,
    removed: Vec<Symbol>,
}

impl SecurityChangesView {
    pub fn new() -> Self {
        Self {
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    pub fn from_symbols(added: Vec<Symbol>, removed: Vec<Symbol>) -> Self {
        Self { added, removed }
    }
    pub fn added_symbols(&self) -> Vec<Symbol> {
        self.added.clone()
    }
    pub fn added_securities(&self) -> Vec<crate::securities::SecurityHandle> {
        self.added
            .iter()
            .cloned()
            .map(crate::securities::SecurityHandle::new)
            .collect()
    }
    pub fn removed_securities(&self) -> Vec<crate::securities::SecurityHandle> {
        self.removed
            .iter()
            .cloned()
            .map(crate::securities::SecurityHandle::new)
            .collect()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl SecurityChangesView {
    #[getter(added_securities)]
    fn py_added_securities(&self) -> Vec<crate::securities::SecurityHandle> {
        self.added_securities()
    }

    #[getter(removed_securities)]
    fn py_removed_securities(&self) -> Vec<crate::securities::SecurityHandle> {
        self.removed_securities()
    }
}

impl SecurityChangesView {
    pub fn removed_symbols(&self) -> Vec<Symbol> {
        self.removed.clone()
    }
}

impl Default for SecurityChangesView {
    fn default() -> Self {
        Self::new()
    }
}

impl ScheduledUniverseDescriptor {
    pub fn scheduled(date_rule: DateRule, time_rule: TimeRule, settings: UniverseSettings) -> Self {
        Self {
            date_rule,
            time_rule,
            settings,
            trigger_resolution: Resolution::Daily,
            symbol_security_type: SecurityType::Equity,
            symbol_market: Market::usa(),
            custom_source_type: None,
            custom_ticker: None,
            custom_properties: HashMap::new(),
        }
    }

    pub fn user_defined(
        trigger_resolution: Resolution,
        settings: UniverseSettings,
        symbol_security_type: SecurityType,
        symbol_market: Market,
    ) -> Self {
        let mut descriptor =
            Self::scheduled(DateRule::EveryDay, TimeRule::EveryResolution, settings);
        descriptor.settings.resolution = trigger_resolution;
        descriptor.trigger_resolution = trigger_resolution;
        descriptor.symbol_security_type = symbol_security_type;
        descriptor.symbol_market = symbol_market;
        descriptor
    }

    pub fn custom_data(
        source_type: String,
        ticker: String,
        trigger_resolution: Resolution,
        settings: UniverseSettings,
        symbol_security_type: SecurityType,
        symbol_market: Market,
    ) -> Self {
        let mut descriptor =
            Self::scheduled(DateRule::EveryDay, TimeRule::EveryResolution, settings);
        descriptor.trigger_resolution = trigger_resolution;
        descriptor.symbol_security_type = symbol_security_type;
        descriptor.symbol_market = symbol_market;
        descriptor.custom_source_type = Some(source_type);
        descriptor.custom_ticker = Some(ticker.to_uppercase());
        descriptor
    }

    pub fn with_custom_properties(mut self, properties: HashMap<String, String>) -> Self {
        self.custom_properties = properties;
        self
    }

    pub fn live_universe_subscription(&self) -> Option<LiveUniverseSubscriptionConfig> {
        Some(LiveUniverseSubscriptionConfig {
            source_type: self.custom_source_type.as_ref()?.clone(),
            ticker: self.custom_ticker.as_ref()?.clone(),
            resolution: self.trigger_resolution,
            properties: self.custom_properties.clone(),
        })
    }

    pub fn is_custom_data(&self) -> bool {
        self.custom_ticker.is_some()
    }

    pub fn create_symbol(&self, ticker: &str) -> Symbol {
        create_symbol_for_universe(ticker, self.symbol_security_type, &self.symbol_market)
    }
}

pub fn create_symbol_for_universe(
    ticker: &str,
    security_type: SecurityType,
    market: &Market,
) -> Symbol {
    match security_type {
        SecurityType::Crypto => Symbol::create_crypto(ticker, market),
        SecurityType::CryptoFuture => Symbol::create_crypto_future(ticker, market),
        SecurityType::Forex => Symbol::create_forex(ticker),
        SecurityType::Equity => Symbol::create_equity(ticker, market),
        _ => Symbol::create_equity(ticker, market),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universe_settings_handle_projects_defaults_and_mutations() {
        let settings = UniverseSettingsHandle::new();

        assert_eq!(settings.resolution(), Resolution::Daily);
        assert_eq!(settings.leverage(), 1.0);
        assert!(settings.fill_forward());
        assert!(!settings.extended_market_hours());
        assert_eq!(settings.minimum_time_in_universe(), 0.0);
        assert_eq!(
            settings.data_normalization_mode(),
            DataNormalizationMode::Adjusted
        );

        settings.set_resolution(Resolution::Hour);
        settings.set_leverage(2.5);
        settings.set_fill_forward(false);
        settings.set_extended_market_hours(true);
        settings.set_minimum_time_in_universe(-10.0);
        settings.set_data_normalization_mode(DataNormalizationMode::Raw);

        let snapshot = settings.snapshot();
        assert_eq!(snapshot.resolution, Resolution::Hour);
        assert_eq!(snapshot.leverage, 2.5);
        assert!(!snapshot.fill_forward);
        assert!(snapshot.extended_market_hours);
        assert_eq!(snapshot.minimum_time_in_universe_secs, 0.0);
        assert_eq!(snapshot.data_normalization_mode, DataNormalizationMode::Raw);
    }

    #[test]
    fn date_time_rules_and_scheduled_handle_store_descriptor_shape() {
        let date_rule = DateRulesHandle::new().every_day();
        let time_rule = TimeRulesHandle::new().at(9, 31, 45);
        let universe = ScheduledUniverseHandle::new(
            date_rule,
            time_rule,
            Some(UniverseSettings {
                resolution: Resolution::Minute,
                ..UniverseSettings::default()
            }),
        );

        let descriptor = universe.descriptor();
        assert_eq!(descriptor.date_rule, DateRule::EveryDay);
        assert_eq!(
            descriptor.time_rule,
            TimeRule::At {
                hour: 9,
                minute: 31
            }
        );
        assert_eq!(universe.settings().resolution, Resolution::Minute);

        let after_open = TimeRulesHandle::new().after_market_open(15);
        assert_eq!(
            after_open.kind,
            TimeRule::AfterMarketOpen {
                minutes_after_open: 15
            }
        );
    }

    #[test]
    fn security_changes_view_returns_added_and_removed_symbols() {
        let added = Symbol::create_equity("SPY", &Market::usa());
        let removed = Symbol::create_crypto("BTCUSD", &Market::new("coinbase"));
        let changes = SecurityChangesView::from_symbols(vec![added.clone()], vec![removed.clone()]);

        assert_eq!(changes.added_symbols(), vec![added]);
        assert_eq!(changes.removed_symbols(), vec![removed]);
        assert!(SecurityChangesView::new().added_symbols().is_empty());
    }

    #[test]
    fn custom_data_descriptor_builds_live_subscription() {
        let mut properties = HashMap::new();
        properties.insert("market".to_string(), Market::HYPERLIQUID.to_string());
        let descriptor = ScheduledUniverseDescriptor::custom_data(
            "hyperliquid".to_string(),
            "hip3_xyz".to_string(),
            Resolution::Hour,
            UniverseSettings::default(),
            SecurityType::CryptoFuture,
            Market::hyperliquid(),
        )
        .with_custom_properties(properties.clone());

        let subscription = descriptor.live_universe_subscription().unwrap();
        assert_eq!(subscription.source_type, "hyperliquid");
        assert_eq!(subscription.ticker, "HIP3_XYZ");
        assert_eq!(subscription.resolution, Resolution::Hour);
        assert_eq!(subscription.properties, properties);
    }

    #[test]
    fn descriptor_converts_string_symbols_with_security_type_and_market() {
        let descriptor = ScheduledUniverseDescriptor::custom_data(
            "hyperliquid".to_string(),
            "HIP3_XYZ".to_string(),
            Resolution::Hour,
            UniverseSettings::default(),
            SecurityType::CryptoFuture,
            Market::hyperliquid(),
        );

        let symbol = descriptor.create_symbol("BTC");

        assert_eq!(symbol.security_type(), SecurityType::CryptoFuture);
        assert_eq!(symbol.market().as_str(), Market::HYPERLIQUID);
    }
}
