use lean_core::DateTime;
use parking_lot::Mutex;

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub time: DateTime,
    pub message: String,
    pub level: LogLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel { Debug, Info, Warning, Error }

pub struct AlgorithmLogging {
    entries: Mutex<Vec<LogEntry>>,
    max_entries: usize,
}

impl AlgorithmLogging {
    pub fn new(max_entries: usize) -> Self {
        AlgorithmLogging { entries: Mutex::new(Vec::new()), max_entries }
    }

    pub fn log(&self, time: DateTime, message: String, level: LogLevel) {
        // Emit immediately via tracing so the message appears in real-time output.
        match level {
            LogLevel::Debug   => tracing::debug!("[Algorithm] {}", message),
            LogLevel::Info    => tracing::info!(target: "algorithm", "{}", message),
            LogLevel::Warning => tracing::warn!("[Algorithm] {}", message),
            LogLevel::Error   => tracing::error!("[Algorithm] {}", message),
        }
        let mut entries = self.entries.lock();
        if entries.len() >= self.max_entries { entries.remove(0); }
        entries.push(LogEntry { time, message, level });
    }

    pub fn debug(&self, time: DateTime, msg: impl Into<String>) {
        self.log(time, msg.into(), LogLevel::Debug);
    }
    pub fn info(&self, time: DateTime, msg: impl Into<String>) {
        self.log(time, msg.into(), LogLevel::Info);
    }
    pub fn warn(&self, time: DateTime, msg: impl Into<String>) {
        self.log(time, msg.into(), LogLevel::Warning);
    }
    pub fn error(&self, time: DateTime, msg: impl Into<String>) {
        self.log(time, msg.into(), LogLevel::Error);
    }

    pub fn entries(&self) -> Vec<LogEntry> {
        self.entries.lock().clone()
    }
}

impl Default for AlgorithmLogging {
    fn default() -> Self { AlgorithmLogging::new(100_000) }
}
