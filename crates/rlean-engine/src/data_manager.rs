use crate::data_feed::DataFeedContext;
use crate::slice_synchronizer::SliceSynchronizer;
use crate::subscription_reader::SubscriptionStream;
use rlean_core::{DateTime, Result as LeanResult};
use rlean_data::{Slice, SubscriptionDataConfig};
use std::collections::HashMap;

/// Drives backtest data through registered Flight subscriptions.
pub struct DataManager {
    context: DataFeedContext,
    active_subscriptions: HashMap<u64, SubscriptionDataConfig>,
    synchronizer: Option<SliceSynchronizer>,
    end: Option<DateTime>,
}

impl DataManager {
    pub fn from_context(context: DataFeedContext) -> Self {
        Self {
            context,
            active_subscriptions: HashMap::new(),
            synchronizer: None,
            end: None,
        }
    }

    pub fn context(&self) -> &DataFeedContext {
        &self.context
    }

    pub async fn initialize_feed(
        &mut self,
        configs: &[SubscriptionDataConfig],
        start: DateTime,
        end: DateTime,
    ) -> LeanResult<()> {
        self.active_subscriptions.clear();
        let streams = configs
            .iter()
            .cloned()
            .map(|config| {
                self.active_subscriptions
                    .insert(config.unique_id(), config.clone());
                SubscriptionStream::new(config, self.context.clone(), start, end)
            })
            .collect();
        self.synchronizer = Some(SliceSynchronizer::new(streams, end));
        self.end = Some(end);
        Ok(())
    }

    pub fn add_subscription(&mut self, config: SubscriptionDataConfig, start: DateTime) {
        let id = config.unique_id();
        if self.active_subscriptions.contains_key(&id) {
            return;
        }
        let end = self.end.unwrap_or(start);
        let stream =
            SubscriptionStream::new_dynamic(config.clone(), self.context.clone(), start, end);
        self.active_subscriptions.insert(id, config);
        match self.synchronizer.as_mut() {
            Some(sync) => sync.add_stream(stream),
            None => {
                self.synchronizer = Some(SliceSynchronizer::new(vec![stream], end));
            }
        }
    }

    pub async fn add_subscription_async(
        &mut self,
        config: SubscriptionDataConfig,
        start: DateTime,
    ) -> LeanResult<()> {
        self.add_subscription(config, start);
        Ok(())
    }

    pub async fn add_subscriptions_async(
        &mut self,
        configs: Vec<SubscriptionDataConfig>,
        start: DateTime,
    ) -> LeanResult<()> {
        for config in configs {
            self.add_subscription(config, start);
        }
        Ok(())
    }

    pub fn remove_subscription(&mut self, config: &SubscriptionDataConfig) {
        let id = config.unique_id();
        if self.active_subscriptions.remove(&id).is_some() {
            if let Some(sync) = self.synchronizer.as_mut() {
                sync.remove_stream(id);
            }
        }
    }

    pub async fn next_slice(&mut self) -> LeanResult<Option<Slice>> {
        let slice = match self.synchronizer.as_mut() {
            Some(sync) => sync.next_slice().await?,
            None => None,
        };
        Ok(slice)
    }
}
