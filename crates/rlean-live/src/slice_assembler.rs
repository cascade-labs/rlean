use rlean_core::DateTime;
use rlean_data::{LiveDataItem, Slice};

/// Converts live provider events into monotonic LEAN `Slice` frontiers.
#[derive(Debug, Default)]
pub struct LiveSliceAssembler {
    current_time: Option<DateTime>,
    current: Option<Slice>,
}

impl LiveSliceAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, item: LiveDataItem) -> Vec<Slice> {
        let item_time = item.end_time();
        match self.current_time {
            None => {
                let mut slice = Slice::new(item_time);
                item.add_to_slice(&mut slice);
                self.current_time = Some(item_time);
                self.current = Some(slice);
                Vec::new()
            }
            Some(current_time) if item_time == current_time => {
                if let Some(slice) = &mut self.current {
                    item.add_to_slice(slice);
                }
                Vec::new()
            }
            Some(current_time) if item_time > current_time => {
                let completed = self.current.take();
                let mut next = Slice::new(item_time);
                item.add_to_slice(&mut next);
                self.current_time = Some(item_time);
                self.current = Some(next);
                completed
                    .into_iter()
                    .filter(|slice| slice.has_data)
                    .collect()
            }
            Some(_) => {
                let mut late = Slice::new(item_time);
                item.add_to_slice(&mut late);
                if late.has_data {
                    vec![late]
                } else {
                    Vec::new()
                }
            }
        }
    }

    pub fn flush(&mut self) -> Option<Slice> {
        self.current_time = None;
        self.current.take().filter(|slice| slice.has_data)
    }

    pub fn flush_ready(&mut self, now: DateTime) -> Option<Slice> {
        if self
            .current_time
            .map(|current_time| current_time <= now)
            .unwrap_or(false)
        {
            self.flush()
        } else {
            None
        }
    }
}
