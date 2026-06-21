use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A bounded, lock-free MPSC queue backed by `crossbeam_queue::ArrayQueue`.
/// When the queue is full, `push` displaces the oldest element (front) to make room.
///
/// The internals can be replaced with a hand-rolled implementation without
/// changing any call sites — the public API is the only contract.
pub(crate) struct MpscQueue<T> {
    inner: ArrayQueue<T>,
    /// Cumulative count of elements displaced by `push` due to overflow.
    dropped: AtomicUsize,
}

impl<T> MpscQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: ArrayQueue::new(capacity),
            dropped: AtomicUsize::new(0),
        }
    }

    /// Push a value, displacing the oldest element if the queue is at capacity.
    /// Returns `true` if a drop occurred (also reflected in `dropped()`).
    pub fn push(&self, value: T) -> bool {
        let displaced = self.inner.force_push(value).is_some();
        if displaced {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        displaced
    }

    /// Cumulative number of elements ever displaced by `push` due to overflow.
    pub fn dropped(&self) -> usize {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn pop(&self) -> Option<T> {
        self.inner.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn clear(&self) {
        while self.pop().is_some() {
            // Keep popping while there are inputs
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}
