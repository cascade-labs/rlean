use crate::{DateRule, TimeRule};
use parking_lot::Mutex;
use rlean_core::DateTime;
use std::sync::Arc;

pub type ScheduledCallback = Arc<Mutex<Box<dyn FnMut() -> Result<(), String> + Send>>>;

pub struct ScheduledEvent {
    pub name: String,
    pub date_rule: DateRule,
    pub time_rule: TimeRule,
    pub callback: ScheduledCallback,
    pub enabled: bool,
    pub last_evaluated: DateTime,
}

impl ScheduledEvent {
    pub fn new(
        name: impl Into<String>,
        date_rule: DateRule,
        time_rule: TimeRule,
        callback: impl FnMut() -> Result<(), String> + Send + 'static,
        last_evaluated: DateTime,
    ) -> Self {
        ScheduledEvent {
            name: name.into(),
            date_rule,
            time_rule,
            callback: Arc::new(Mutex::new(Box::new(callback))),
            enabled: true,
            last_evaluated,
        }
    }
}

impl std::fmt::Debug for ScheduledEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScheduledEvent({})", self.name)
    }
}
