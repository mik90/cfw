use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A bounded, lock-free MPSC queue backed by `crossbeam_queue::ArrayQueue`.
/// When the queue is full, `push` displaces the oldest element (front) to make room.
///
/// The internals can be replaced with a hand-rolled implementation without
/// changing any call sites — the public API is the only contract.
pub(crate) struct MpscQueue<T> {
    inner: ArrayQueue<T>,
    drops: AtomicUsize,
}

impl<T> MpscQueue<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: ArrayQueue::new(capacity),
            drops: AtomicUsize::new(0),
        }
    }

    /// Push a value, displacing the oldest element if the queue is at capacity.
    /// Returns `true` if a drop occurred.
    pub fn push(&self, value: T) -> bool {
        if self.inner.force_push(value).is_some() {
            self.drops.fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    pub fn pop(&self) -> Option<T> {
        self.inner.pop()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn drops(&self) -> usize {
        self.drops.load(Ordering::Relaxed)
    }
}
