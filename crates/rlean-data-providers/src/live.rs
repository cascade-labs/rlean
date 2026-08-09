use anyhow::Result;
use async_trait::async_trait;
use futures::stream::BoxStream;
use rlean_data::SubscriptionDataConfig;

use crate::HistoricalData;

#[derive(Debug, Clone)]
pub struct LiveSubscription {
    pub id: u64,
    pub configuration: SubscriptionDataConfig,
}

#[derive(Debug, Clone)]
pub enum LiveDataEvent {
    Data {
        subscription_id: u64,
        data: HistoricalData,
    },
    Reconnected,
    Disconnected {
        reason: String,
    },
}

/// LEAN `IDataQueueHandler` equivalent. Adding/removing subscriptions mutates
/// provider membership; data arrives unsolicited through `events`.
#[async_trait]
pub trait LiveDataProvider: Send + Sync {
    fn name(&self) -> &str;
    fn is_connected(&self) -> bool;
    async fn connect(&self) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;
    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()>;
    async fn unsubscribe(&self, subscription_id: u64) -> Result<()>;
    async fn events(&self) -> Result<BoxStream<'static, Result<LiveDataEvent>>>;
}
