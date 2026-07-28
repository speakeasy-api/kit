use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidQueueBound;

#[derive(Debug, Eq, PartialEq)]
pub struct QueueFull<T> {
    item: T,
    capacity: usize,
}

impl<T> QueueFull<T> {
    pub fn into_item(self) -> T {
        self.item
    }

    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

pub struct AdmissionQueue<T> {
    capacity: usize,
    queue: Mutex<VecDeque<T>>,
}

impl<T> AdmissionQueue<T> {
    pub fn new(capacity: usize) -> Result<Self, InvalidQueueBound> {
        if capacity == 0 {
            return Err(InvalidQueueBound);
        }
        Ok(Self {
            capacity,
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
        })
    }

    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn try_admit(&self, item: T) -> Result<(), QueueFull<T>> {
        let mut queue = self.lock();
        if queue.len() >= self.capacity {
            return Err(QueueFull {
                item,
                capacity: self.capacity,
            });
        }
        queue.push_back(item);
        Ok(())
    }

    pub fn pop(&self) -> Option<T> {
        self.lock().pop_front()
    }

    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<T>> {
        self.queue
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
