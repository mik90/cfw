use crossbeam_queue::ArrayQueue;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A bounded, lock-free MPSC queue backed by `crossbeam_queue::ArrayQueue`.
/// When the queue is full, `push` displaces the oldest element (front) to make room.
pub struct MpscQueue<T> {
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
    /// Returns `false` if a displacement occurred (also reflected in `dropped()`).
    pub fn push(&self, value: T) -> bool {
        let displaced = self.inner.force_push(value).is_some();
        if displaced {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        !displaced
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
        while self.pop().is_some() {}
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use crate::mpsc_queue::MpscQueue;

    #[test]
    fn test_push_pop() {
        let queue = MpscQueue::<usize>::new(1);
        assert!(queue.push(1));
        assert_eq!(queue.pop(), Some(1));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn test_capacity_one() {
        let queue = MpscQueue::<usize>::new(1);

        assert!(queue.push(1));
        // Queue full; push displaces oldest.
        assert!(!queue.push(2)); // displacement occurred
        assert_eq!(queue.dropped(), 1);
        assert_eq!(queue.pop(), Some(2));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_multi_producer() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let n_producers = 4;
        #[cfg(not(miri))]
        let n_per_producer = 50;
        #[cfg(miri)] // miri is slower
        let n_per_producer = 10;

        let queue = Arc::new(MpscQueue::<usize>::new(256));
        let start = Arc::new(Barrier::new(n_producers));
        let done = Arc::new(Barrier::new(n_producers + 1));

        let mut handles = Vec::new();
        for thread_id in 0..n_producers {
            let queue = Arc::clone(&queue);
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            handles.push(thread::spawn(move || {
                start.wait();
                for i in 0..n_per_producer {
                    queue.push(thread_id * n_per_producer + i);
                }
                done.wait();
            }));
        }

        // Consumer runs concurrently.
        let mut popped = vec![0usize; n_producers];
        let mut count = 0;
        while count < n_producers * n_per_producer {
            if let Some(value) = queue.pop() {
                let thread_id = value / n_per_producer;
                assert!(thread_id < n_producers);
                popped[thread_id] += 1;
                count += 1;
            }
        }
        done.wait();

        for handle in handles {
            handle.join().unwrap();
        }
        for (id, c) in popped.iter().enumerate() {
            assert_eq!(*c, n_per_producer, "thread {id} missing");
        }
        assert!(queue.is_empty());
    }
}
