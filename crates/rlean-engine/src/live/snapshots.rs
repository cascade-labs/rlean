use chrono::{DateTime, Utc};
use rlean_orders::{Order, OrderEvent};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveDeploymentSnapshot {
    pub deployment_id: String,
    pub timestamp: DateTime<Utc>,
    pub slices_processed: usize,
    pub final_value: f64,
    pub open_orders: Vec<Order>,
    pub recent_order_events: Vec<OrderEvent>,
}

impl LiveDeploymentSnapshot {
    pub fn new(deployment_id: String) -> Self {
        Self {
            deployment_id,
            timestamp: Utc::now(),
            slices_processed: 0,
            final_value: 0.0,
            open_orders: Vec::new(),
            recent_order_events: Vec::new(),
        }
    }
}
