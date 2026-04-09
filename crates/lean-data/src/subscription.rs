use lean_core::{DataNormalizationMode, Resolution, SecurityType, Symbol, TickType};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use parking_lot::RwLock;
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
        use std::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;
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
    subscriptions: RwLock<HashMap<u64, Arc<SubscriptionDataConfig>>>,
}

impl SubscriptionManager {
    pub fn new() -> Self {
        SubscriptionManager::default()
    }

    pub fn add(&self, config: SubscriptionDataConfig) -> Arc<SubscriptionDataConfig> {
        let id = config.unique_id();
        let config = Arc::new(config);
        self.subscriptions.write().insert(id, config.clone());
        config
    }

    pub fn remove(&self, config: &SubscriptionDataConfig) {
        self.subscriptions.write().remove(&config.unique_id());
    }

    pub fn get_all(&self) -> Vec<Arc<SubscriptionDataConfig>> {
        self.subscriptions.read().values().cloned().collect()
    }

    pub fn count(&self) -> usize {
        self.subscriptions.read().len()
    }
}
