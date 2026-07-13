use std::collections::VecDeque;

/// Fixed-capacity rolling window — O(1) push and indexed access.
#[derive(Debug, Clone)]
pub struct RollingWindow<T: Clone> {
    data: VecDeque<T>,
    capacity: usize,
}

impl<T: Clone> RollingWindow<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "RollingWindow capacity must be > 0");
        RollingWindow {
            data: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn push(&mut self, item: T) {
        if self.data.len() == self.capacity {
            self.data.pop_back();
        }
        self.data.push_front(item);
    }

    /// Most recent value (index 0 = newest).
    pub fn newest(&self) -> Option<&T> {
        self.data.front()
    }

    /// Oldest value currently stored.
    pub fn oldest(&self) -> Option<&T> {
        self.data.back()
    }

    /// Index 0 = newest, index (len-1) = oldest.
    pub fn get(&self, index: usize) -> Option<&T> {
        self.data.get(index)
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_full(&self) -> bool {
        self.data.len() == self.capacity
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn clear(&mut self) {
        self.data.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.data.iter()
    }
}

impl<T: Clone + std::ops::Add<Output = T> + std::ops::Div<Output = T> + Default + From<u32>>
    RollingWindow<T>
{
    pub fn sum(&self) -> T
    where
        T: std::iter::Sum,
    {
        self.data.iter().cloned().sum()
    }
}
