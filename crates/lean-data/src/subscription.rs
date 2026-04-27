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
    pub fn new_equity(symbol: Symbol, resolution: Resolution) -> Self {
        SubscriptionDataConfig {
            symbol,
            resolution,
            tick_type: TickType::Trade,
            normalization_mode: DataNormalizationMode::Adjusted,
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

    pub fn add(&self, config: SubscriptionDataConfig) -> Arc<SubscriptionDataConfig> {
        let id = config.unique_id();
        let config = Arc::new(config);
        let mut state = self.state.write();
        if !state.subscriptions.contains_key(&id) {
            state.order.push(id);
        }
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

    pub fn count(&self) -> usize {
        self.state.read().subscriptions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{SubscriptionDataConfig, SubscriptionManager};
    use lean_core::{Market, Resolution, Symbol};

    fn equity_config(ticker: &str) -> SubscriptionDataConfig {
        SubscriptionDataConfig::new_equity(
            Symbol::create_equity(ticker, &Market::new(Market::USA)),
            Resolution::Minute,
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
}
