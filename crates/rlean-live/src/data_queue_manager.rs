use rlean_core::{LeanError, Result};
use rlean_data::{
    DataQueueHandler, LiveDataSubscription, LiveNodePacket, LiveSubscriptionKey,
    LiveUniverseSubscriptionConfig, SubscriptionDataConfig,
};
use std::collections::HashMap;

/// Stack of live data queue handlers.
///
/// LEAN live deployments can specify multiple queue handlers. The first handler
/// that accepts a subscription owns it until unsubscribe. Handlers signal
/// "try the next provider" with `LeanError::Unsupported`.
pub struct DataQueueHandlerManager {
    handlers: Vec<Box<dyn DataQueueHandler>>,
    owners: HashMap<LiveSubscriptionKey, usize>,
}

impl DataQueueHandlerManager {
    pub fn new(handlers: Vec<Box<dyn DataQueueHandler>>) -> Self {
        Self {
            handlers,
            owners: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.handlers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    pub fn set_job(&mut self, job: &LiveNodePacket) -> Result<()> {
        for handler in &mut self.handlers {
            handler.set_job(job)?;
        }
        Ok(())
    }

    pub fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
        let key = LiveSubscriptionKey::Market(config.unique_id());
        if self.owners.contains_key(&key) {
            return Err(LeanError::DataError(format!(
                "live subscription already exists for {}",
                config.symbol
            )));
        }

        let mut unsupported = Vec::new();
        for (index, handler) in self.handlers.iter_mut().enumerate() {
            match handler.subscribe(config) {
                Ok(subscription) => {
                    self.owners.insert(key, index);
                    return Ok(subscription);
                }
                Err(LeanError::Unsupported(reason)) => unsupported.push(reason),
                Err(err) => return Err(err),
            }
        }

        Err(LeanError::Unsupported(format!(
            "no live data queue handler supports {} ({:?}); tried: {}",
            config.symbol,
            config.tick_type,
            unsupported.join("; ")
        )))
    }

    pub fn subscribe_universe(
        &mut self,
        subscription: &LiveUniverseSubscriptionConfig,
    ) -> Result<LiveDataSubscription> {
        let key = LiveSubscriptionKey::Universe {
            source_type: subscription.source_type.clone(),
            ticker: subscription.ticker.clone(),
        };
        if self.owners.contains_key(&key) {
            return Err(LeanError::DataError(format!(
                "live universe subscription already exists for {}:{}",
                subscription.source_type, subscription.ticker
            )));
        }

        let mut unsupported = Vec::new();
        for (index, handler) in self.handlers.iter_mut().enumerate() {
            match handler.subscribe_universe(subscription) {
                Ok(live_subscription) => {
                    self.owners.insert(key, index);
                    return Ok(live_subscription);
                }
                Err(LeanError::Unsupported(reason)) => unsupported.push(reason),
                Err(err) => return Err(err),
            }
        }

        Err(LeanError::Unsupported(format!(
            "no live data queue handler supports universe subscription {}:{}; tried: {}",
            subscription.source_type,
            subscription.ticker,
            unsupported.join("; ")
        )))
    }

    pub fn unsubscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()> {
        let key = LiveSubscriptionKey::Market(config.unique_id());
        let Some(owner) = self.owners.remove(&key) else {
            return Ok(());
        };
        self.handlers[owner].unsubscribe(config)
    }

    pub fn unsubscribe_universe(
        &mut self,
        subscription: &LiveUniverseSubscriptionConfig,
    ) -> Result<()> {
        let key = LiveSubscriptionKey::Universe {
            source_type: subscription.source_type.clone(),
            ticker: subscription.ticker.clone(),
        };
        let Some(owner) = self.owners.remove(&key) else {
            return Ok(());
        };
        self.handlers[owner].unsubscribe_universe(subscription)
    }

    pub fn is_connected(&self) -> bool {
        if self.handlers.is_empty() {
            return false;
        }
        if self.owners.is_empty() {
            return self.handlers.iter().all(|handler| handler.is_connected());
        }

        self.owners
            .values()
            .copied()
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .all(|owner| self.handlers[owner].is_connected())
    }
}
