//! Batch-buffered reporter adapter.
//!
//! [`BufferedReporter`] enqueues events and forwards them to an inner
//! [`LoopObserver`] in batches — either when the buffer reaches a configured
//! capacity or on an explicit [`flush`](BufferedReporter::flush) call.
//! Remaining events are flushed automatically on drop.

use agentkit_loop::{LoopObserver, ObservedEvent};

/// Reporter adapter that enqueues events for batch flushing.
///
/// Wraps any [`LoopObserver`] and delivers events in batches rather than
/// one-at-a-time. This is useful when the inner reporter benefits from
/// amortised work (e.g. a network sink that batches writes).
///
/// # Example
///
/// ```rust
/// use agentkit_reporting::{BufferedReporter, UsageReporter};
///
/// // Buffer up to 64 events before forwarding to the inner reporter.
/// let reporter = BufferedReporter::new(UsageReporter::new(), 64);
/// ```
pub struct BufferedReporter<T: LoopObserver> {
    inner: T,
    buffer: std::sync::Mutex<Vec<ObservedEvent>>,
    capacity: usize,
}

impl<T: LoopObserver> BufferedReporter<T> {
    /// Creates a new `BufferedReporter` that buffers up to `capacity` events
    /// before flushing them to `inner`.
    pub fn new(inner: T, capacity: usize) -> Self {
        Self {
            inner,
            buffer: std::sync::Mutex::new(Vec::with_capacity(capacity)),
            capacity,
        }
    }

    /// Delivers all buffered events to the inner observer and clears the
    /// buffer.
    pub fn flush(&self) {
        let drained = std::mem::replace(
            &mut *self.buffer.lock().unwrap_or_else(|e| e.into_inner()),
            Vec::with_capacity(self.capacity),
        );
        for event in drained {
            self.inner.handle_event(event);
        }
    }

    /// Returns the number of events currently buffered.
    pub fn pending(&self) -> usize {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Returns a reference to the inner observer.
    pub fn inner(&self) -> &T {
        &self.inner
    }
}

impl<T: LoopObserver> LoopObserver for BufferedReporter<T> {
    fn handle_event(&self, event: ObservedEvent) {
        let needs_flush = {
            let mut buffer = self.buffer.lock().unwrap_or_else(|e| e.into_inner());
            buffer.push(event);
            self.capacity > 0 && buffer.len() >= self.capacity
        };
        if needs_flush {
            self.flush();
        }
    }
}

impl<T: LoopObserver> Drop for BufferedReporter<T> {
    fn drop(&mut self) {
        self.flush();
    }
}
