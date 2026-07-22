use rlean_core::DateTime;
use rlean_data::{LiveDataItem, Slice};

/// Synchronizes provider events against a monotonic live-time frontier.
///
/// Provider timestamps describe when data occurred. They do not control the
/// algorithm clock: late data is delivered at the current frontier and future
/// data waits until the frontier reaches it, matching LEAN's live synchronizer.
#[derive(Debug, Default)]
pub struct LiveSliceAssembler {
    pending: Vec<LiveDataItem>,
    last_frontier: Option<DateTime>,
}

impl LiveSliceAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds provider data without advancing algorithm time.
    pub fn enqueue(&mut self, item: LiveDataItem) {
        self.pending.push(item);
    }

    /// Advances live time and emits all data whose availability time is at or
    /// behind that frontier. A caller-supplied clock regression is clamped to
    /// the last frontier, so this type can never emit a backward `Slice`.
    pub fn advance(&mut self, frontier: DateTime) -> Option<Slice> {
        let frontier = self
            .last_frontier
            .map_or(frontier, |last| frontier.max(last));
        self.last_frontier = Some(frontier);

        let mut due = Vec::new();
        let mut future = Vec::new();
        for item in self.pending.drain(..) {
            if item.end_time() <= frontier {
                due.push(item);
            } else {
                future.push(item);
            }
        }
        self.pending = future;

        if due.is_empty() {
            return None;
        }

        let mut slice = Slice::new(frontier);
        for item in due {
            item.add_to_slice(&mut slice);
        }
        slice.has_data.then_some(slice)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}
