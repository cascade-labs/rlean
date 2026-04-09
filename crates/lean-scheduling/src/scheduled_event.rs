use lean_core::DateTime;

pub struct ScheduledEvent {
    pub name: String,
    pub callback: Box<dyn FnMut() + Send + Sync>,
    pub next_fire_time: DateTime,
    pub enabled: bool,
}

impl ScheduledEvent {
    pub fn new(name: impl Into<String>, callback: impl FnMut() + Send + Sync + 'static, next_fire: DateTime) -> Self {
        ScheduledEvent {
            name: name.into(),
            callback: Box::new(callback),
            next_fire_time: next_fire,
            enabled: true,
        }
    }

    pub fn fire(&mut self) {
        if self.enabled { (self.callback)(); }
    }
}

impl std::fmt::Debug for ScheduledEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScheduledEvent({})", self.name)
    }
}
