use lean_core::{DataNormalizationMode, Resolution, Symbol, TickType};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// All configuration needed to subscribe to a data stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionDataConfig {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub tick_type: TickType,
    pub normalization_mode: DataNormalizationMode,
    pub fill_data_forward: bool,
    pub extended_market_hours: bool,
    pub is_internal_feed: bool,
    pub is_filtered_subscription: bool,
    pub data_time_zone: String,
    pub exchange_time_zone: String,
}

impl SubscriptionDataConfig {
    pub fn new_equity(
        symbol: Symbol,
        resolution: Resolution,
        normalization_mode: DataNormalizationMode,
    ) -> Self {
        SubscriptionDataConfig {
            symbol,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode,
            fill_data_forward: true,
            extended_market_hours: false,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
        }
    }

    /// Option subscriptions are always Raw — matches C# Lean's `Option`
    /// constructor and `DataManager.Add` forcing for option/index symbols.
    pub fn new_option(symbol: Symbol, resolution: Resolution) -> Self {
        SubscriptionDataConfig {
            symbol,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: true,
            extended_market_hours: false,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "America/New_York".into(),
            exchange_time_zone: "America/New_York".into(),
        }
    }

    pub fn new_forex(symbol: Symbol, resolution: Resolution) -> Self {
        SubscriptionDataConfig {
            symbol,
            resolution,
            tick_type: TickType::Quote,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: true,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "UTC".into(),
            exchange_time_zone: "UTC".into(),
        }
    }

    pub fn new_crypto(symbol: Symbol, resolution: Resolution) -> Self {
        SubscriptionDataConfig {
            symbol,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: false,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "UTC".into(),
            exchange_time_zone: "UTC".into(),
        }
    }

    pub fn new_crypto_future(symbol: Symbol, resolution: Resolution) -> Self {
        SubscriptionDataConfig {
            symbol,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Raw,
            fill_data_forward: true,
            extended_market_hours: true,
            is_internal_feed: false,
            is_filtered_subscription: false,
            data_time_zone: "UTC".into(),
            exchange_time_zone: "UTC".into(),
        }
    }

    pub fn unique_id(&self) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hash;
        let mut h = DefaultHasher::new();
        self.symbol.id.sid.hash(&mut h);
        (self.resolution as u8).hash(&mut h);
        (self.tick_type as u8).hash(&mut h);
        std::hash::Hasher::finish(&h)
    }
}

/// Manages the set of active subscriptions.
#[derive(Debug, Default)]
pub struct SubscriptionManager {
    state: RwLock<SubscriptionState>,
}

#[derive(Debug, Default)]
struct SubscriptionState {
    subscriptions: HashMap<u64, Arc<SubscriptionDataConfig>>,
    order: Vec<u64>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        SubscriptionManager::default()
    }

    /// Get-or-add: matches C# Lean's `DataManager.SubscriptionManagerGetOrAdd`.
    /// If a config with the same `unique_id` already exists, the existing
    /// `Arc` is returned and the incoming `config` (including its
    /// `normalization_mode`) is discarded. Mode changes must go through
    /// [`set_normalization_mode`].
    pub fn add(&self, config: SubscriptionDataConfig) -> Arc<SubscriptionDataConfig> {
        let id = config.unique_id();
        let mut state = self.state.write();
        if let Some(existing) = state.subscriptions.get(&id) {
            return existing.clone();
        }
        let config = Arc::new(config);
        state.order.push(id);
        state.subscriptions.insert(id, config.clone());
        config
    }

    pub fn remove(&self, config: &SubscriptionDataConfig) {
        let id = config.unique_id();
        let mut state = self.state.write();
        state.subscriptions.remove(&id);
        state.order.retain(|existing_id| *existing_id != id);
    }

    pub fn remove_symbol(&self, symbol: &Symbol) {
        let mut state = self.state.write();
        state
            .subscriptions
            .retain(|_, config| config.symbol.id.sid != symbol.id.sid);
        let active_ids: HashSet<_> = state.subscriptions.keys().copied().collect();
        state.order.retain(|id| active_ids.contains(id));
    }

    pub fn get_all(&self) -> Vec<Arc<SubscriptionDataConfig>> {
        let state = self.state.read();
        state
            .order
            .iter()
            .filter_map(|id| state.subscriptions.get(id).cloned())
            .collect()
    }

    /// All configs for the given symbol (matched by SID) in insertion order.
    /// Mirrors C# Lean's `DataManager.GetSubscriptionDataConfigs(symbol)`.
    pub fn get_configs_for_symbol(&self, symbol: &Symbol) -> Vec<Arc<SubscriptionDataConfig>> {
        let state = self.state.read();
        state
            .order
            .iter()
            .filter_map(|id| state.subscriptions.get(id))
            .filter(|config| config.symbol.id.sid == symbol.id.sid)
            .cloned()
            .collect()
    }

    /// Update the normalization mode of every config matching the symbol.
    /// Mirrors C# Lean's `Security.SetDataNormalizationMode` (which mutates
    /// all attached subscription configs in place). Returns the number of
    /// configs that were updated.
    pub fn set_normalization_mode(
        &self,
        symbol: &Symbol,
        normalization_mode: DataNormalizationMode,
    ) -> usize {
        let mut state = self.state.write();
        let ids: Vec<u64> = state
            .order
            .iter()
            .copied()
            .filter(|id| {
                state
                    .subscriptions
                    .get(id)
                    .map(|config| config.symbol.id.sid == symbol.id.sid)
                    .unwrap_or(false)
            })
            .collect();
        let mut updated = 0;
        for id in ids {
            if let Some(existing) = state.subscriptions.get(&id) {
                if existing.normalization_mode == normalization_mode {
                    continue;
                }
                let mut new_config = (**existing).clone();
                new_config.normalization_mode = normalization_mode;
                state.subscriptions.insert(id, Arc::new(new_config));
                updated += 1;
            }
        }
        updated
    }

    pub fn count(&self) -> usize {
        self.state.read().subscriptions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{SubscriptionDataConfig, SubscriptionManager};
    use lean_core::{
        DataNormalizationMode, Market, OptionRight, OptionStyle, Resolution, Symbol, TickType,
    };
    use rust_decimal_macros::dec;
    use std::sync::Arc;

    fn equity_config(ticker: &str) -> SubscriptionDataConfig {
        SubscriptionDataConfig::new_equity(
            Symbol::create_equity(ticker, &Market::new(Market::USA)),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        )
    }

    #[test]
    fn get_all_preserves_subscription_insertion_order() {
        let manager = SubscriptionManager::new();

        manager.add(equity_config("SPY"));
        manager.add(equity_config("XLK"));
        manager.add(equity_config("XLF"));

        let symbols: Vec<_> = manager
            .get_all()
            .iter()
            .map(|config| config.symbol.value.clone())
            .collect();
        assert_eq!(symbols, vec!["SPY", "XLK", "XLF"]);
    }

    #[test]
    fn removed_subscription_is_removed_from_order() {
        let manager = SubscriptionManager::new();
        let spy = manager.add(equity_config("SPY"));
        manager.add(equity_config("XLK"));
        manager.add(equity_config("XLF"));

        manager.remove(&spy);

        let symbols: Vec<_> = manager
            .get_all()
            .iter()
            .map(|config| config.symbol.value.clone())
            .collect();
        assert_eq!(symbols, vec!["XLK", "XLF"]);
    }

    #[test]
    fn readded_subscription_moves_to_end_after_removal() {
        let manager = SubscriptionManager::new();
        let spy = manager.add(equity_config("SPY"));
        manager.add(equity_config("XLK"));
        manager.remove(&spy);
        manager.add(equity_config("SPY"));

        let symbols: Vec<_> = manager
            .get_all()
            .iter()
            .map(|config| config.symbol.value.clone())
            .collect();
        assert_eq!(symbols, vec!["XLK", "SPY"]);
    }

    #[test]
    fn add_returns_existing_config_when_unique_id_matches() {
        let manager = SubscriptionManager::new();
        let spy = Symbol::create_equity("SPY", &Market::new(Market::USA));
        let first = manager.add(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        ));
        let second = manager.add(SubscriptionDataConfig::new_equity(
            spy,
            Resolution::Minute,
            DataNormalizationMode::Raw,
        ));
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(second.normalization_mode, DataNormalizationMode::Adjusted);
    }

    #[test]
    fn set_normalization_mode_updates_trade_and_quote_configs() {
        let manager = SubscriptionManager::new();
        let spy = Symbol::create_equity("SPY", &Market::new(Market::USA));
        manager.add(SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        ));
        let mut quote_config = SubscriptionDataConfig::new_equity(
            spy.clone(),
            Resolution::Minute,
            DataNormalizationMode::Adjusted,
        );
        quote_config.tick_type = TickType::Quote;
        manager.add(quote_config);

        let updated = manager.set_normalization_mode(&spy, DataNormalizationMode::Raw);
        assert_eq!(updated, 2);
        for config in manager.get_configs_for_symbol(&spy) {
            assert_eq!(config.normalization_mode, DataNormalizationMode::Raw);
        }
    }

    #[test]
    fn new_option_defaults_to_raw_normalization() {
        let underlying = Symbol::create_equity("SPY", &Market::new(Market::USA));
        let option = Symbol::create_option(
            underlying,
            &Market::new(Market::USA),
            chrono::NaiveDate::from_ymd_opt(2026, 1, 16).unwrap(),
            dec!(450),
            OptionRight::Call,
            OptionStyle::American,
        );
        let config = SubscriptionDataConfig::new_option(option, Resolution::Minute);
        assert_eq!(config.normalization_mode, DataNormalizationMode::Raw);
    }
}
