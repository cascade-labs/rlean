use crate::scheduled_event::ScheduledEvent;
use lean_core::DateTime;
use parking_lot::Mutex;

pub struct ScheduleManager {
    events: Mutex<Vec<ScheduledEvent>>,
}

impl ScheduleManager {
    pub fn new() -> Self {
        ScheduleManager { events: Mutex::new(Vec::new()) }
    }

    pub fn add(&self, event: ScheduledEvent) {
        self.events.lock().push(event);
    }

    pub fn scan(&self, utc_time: DateTime) {
        let mut events = self.events.lock();
        for event in events.iter_mut() {
            if event.enabled && utc_time >= event.next_fire_time {
                event.fire();
            }
        }
    }

    pub fn remove(&self, name: &str) {
        self.events.lock().retain(|e| e.name != name);
    }
}

impl Default for ScheduleManager {
    fn default() -> Self { ScheduleManager::new() }
}
