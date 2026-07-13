use rlean_data::SubscriptionDataConfig;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Default, Clone)]
pub struct LiveSubscriptionState {
    pub market: HashMap<u64, Arc<SubscriptionDataConfig>>,
}

impl LiveSubscriptionState {
    pub fn upsert_market(&mut self, subscription: Arc<SubscriptionDataConfig>) {
        self.market.insert(subscription.symbol.id.sid, subscription);
    }

    pub fn remove_market_sid(&mut self, sid: u64) {
        self.market.remove(&sid);
    }
}
