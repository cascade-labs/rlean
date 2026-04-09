use crate::base_data::BaseData;
use crate::subscription::SubscriptionDataConfig;
use lean_core::Result;

/// Trait for live data queue handlers (brokerages, data vendors).
/// Mirrors LEAN's `IDataQueueHandler`.
pub trait DataQueueHandler: Send + Sync {
    fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()>;
    fn unsubscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()>;
    fn get_next_ticks(&mut self) -> Vec<Box<dyn BaseData>>;
    fn is_connected(&self) -> bool;
}
