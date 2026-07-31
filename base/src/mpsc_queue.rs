use crossbeam_queue::ArrayQueue;
use std::cell::UnsafeCell;
use std::hint;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{
    AtomicUsize,
    Ordering::{self, Acquire, Relaxed, Release},
};

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
        while self.pop().is_some() {}
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

// ---------------------------------------------------------------------------
// Hand-rolled experimental queue
// ---------------------------------------------------------------------------

// Cache line size; the common denominator across platforms.
#[repr(align(64))]
#[derive(Debug)]
struct CacheAligned<T> {
    value: T,
}

impl<T> CacheAligned<T> {
    fn new(value: T) -> Self {
        CacheAligned { value }
    }
}

impl<T> Deref for CacheAligned<T> {
    type Target = T;
    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl<T> DerefMut for CacheAligned<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.value
    }
}

impl<T> From<T> for CacheAligned<T> {
    fn from(value: T) -> Self {
        CacheAligned { value }
    }
}

struct Slot<T> {
    sequence: CacheAligned<AtomicUsize>,
    value: UnsafeCell<MaybeUninit<T>>,
}

/// An experimental bounded MPSC queue with force-push semantics.
///
/// When the queue is full, `push` displaces the oldest element to make room.
/// Returns `false` if a displacement occurred (also reflected in `dropped()`).
///
/// This is a hand-rolled ring-buffer similar to crossbeam's `ArrayQueue` but
/// specialised for MPSC. Kept as `pub(crate)` for internal benchmarking and
/// testing; production code should use the crossbeam-backed [`MpscQueue`].
pub struct ExperimentalMpscQueue<T> {
    /// Consumer position; also CAS'ed by producers for displacement.
    head: CacheAligned<AtomicUsize>,
    /// Producer position for the next push.
    tail: CacheAligned<AtomicUsize>,
    /// Cumulative count of displaced elements.
    dropped: CacheAligned<AtomicUsize>,
    capacity: usize,
    slots: Vec<Slot<T>>,
}

impl<T> ExperimentalMpscQueue<T> {
    pub fn new(capacity: usize) -> Self {
        let mut vec = Vec::with_capacity(capacity);
        for i in 0..capacity {
            vec.push(Slot {
                sequence: CacheAligned::new(AtomicUsize::new(i)),
                value: UnsafeCell::new(MaybeUninit::<T>::uninit()),
            });
        }
        Self {
            head: AtomicUsize::new(0).into(),
            tail: AtomicUsize::new(0).into(),
            dropped: AtomicUsize::new(0).into(),
            capacity,
            slots: vec,
        }
    }

    /// Push a value. If the queue is full, the oldest element is displaced.
    /// Returns `false` if a displacement occurred.
    pub fn push(&self, value: T) -> bool {
        let t = self.tail.fetch_add(1, Relaxed);
        let slot = &self.slots[t % self.capacity];
        let mut displaced = false;

        loop {
            let seq = slot.sequence.load(Acquire);
            if seq == t {
                break;
            }

            // Slot is busy. If we are at least capacity positions ahead of the
            // consumer, displace the oldest item to make room.
            let h = self.head.load(Relaxed);
            if t.wrapping_sub(h) >= self.capacity {
                if self
                    .head
                    .compare_exchange_weak(h, h + 1, Acquire, Relaxed)
                    .is_ok()
                {
                    let old = &self.slots[h % self.capacity];
                    old.sequence.store(h + self.capacity, Release);
                    // SAFETY: The displaced slot was written for position h.
                    // The consumer will never read it because head moved past h.
                    unsafe {
                        (*old.value.get()).assume_init_drop();
                    }
                    self.dropped.fetch_add(1, Relaxed);
                    displaced = true;
                }
            }
            hint::spin_loop();
        }

        // SAFETY: The sequence check guarantees exclusive write access.
        unsafe { *slot.value.get() = MaybeUninit::new(value); }
        // Signal the consumer: position t has data.
        slot.sequence.store(t + 1, Release);
        !displaced
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Relaxed)
    }

    pub fn pop(&self) -> Option<T> {
        let mut h = self.head.load(Relaxed);

        loop {
            let t = self.tail.load(Acquire);
            if h == t {
                return None;
            }

            let slot = &self.slots[h % self.capacity];
            let seq = slot.sequence.load(Acquire);
            if seq != h + 1 {
                return None;
            }

            match self
                .head
                .compare_exchange_weak(h, h + 1, Acquire, Relaxed)
            {
                Ok(_) => {
                    let mut return_value = MaybeUninit::uninit();
                    // SAFETY: We own this slot after the CAS. Its data is
                    // initialized (producer published with Release on sequence).
                    unsafe {
                        std::ptr::swap(
                            (*slot.value.get()).as_mut_ptr(),
                            return_value.as_mut_ptr(),
                        );
                    }
                    // Free the slot for the writer at position h + capacity.
                    slot.sequence.store(h + self.capacity, Release);
                    return unsafe { Some(return_value.assume_init()) };
                }
                Err(actual) => {
                    // Head was advanced by a displacing producer – retry.
                    h = actual;
                }
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Relaxed) == self.tail.load(Relaxed)
    }

    pub fn clear(&self) {
        while self.pop().is_some() {}
    }

    pub fn len(&self) -> usize {
        let t = self.tail.load(Relaxed);
        let h = self.head.load(Relaxed);
        t.saturating_sub(h)
    }
}

impl<T> Drop for ExperimentalMpscQueue<T> {
    fn drop(&mut self) {
        let h = self.head.load(Relaxed);
        let t = self.tail.load(Relaxed);
        for pos in h..t {
            let slot = &mut self.slots[pos % self.capacity];
            unsafe { std::ptr::drop_in_place(slot.value.get_mut().as_mut_ptr()); }
        }
    }
}

// SAFETY: The sequence/head/tail protocol ensures that only one thread
// can access a given slot's value at a time, and the reader synchronises
// via Acquire/Release on the `sequence` field.
unsafe impl<T: Send> Sync for ExperimentalMpscQueue<T> {}

#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use crate::mpsc_queue::ExperimentalMpscQueue;

    #[test]
    fn test_push_pop() {
        let queue = ExperimentalMpscQueue::<usize>::new(2);
        assert!(queue.push(1));
        assert_matches!(queue.pop(), Some(1));
    }

    #[test]
    fn test_single_producer_single_consumer() {
        let n = 1_000;
        let queue = ExperimentalMpscQueue::<usize>::new(64);

        for i in 0..n {
            assert!(queue.push(i));
            assert_eq!(queue.pop(), Some(i));
        }
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_multi_producer_single_consumer() {
        let n_producers = 4;
        #[cfg(not(miri))]
        let n_per_producer = 1_000;
        #[cfg(miri)]
        let n_per_producer = 50;

        let total = n_producers * n_per_producer;
        let queue = Arc::new(ExperimentalMpscQueue::<usize>::new(4096));
        let start = Arc::new(Barrier::new(n_producers + 1));
        let done = Arc::new(Barrier::new(n_producers + 1));

        let mut handles = Vec::new();
        for thread_id in 0..n_producers {
            let queue = Arc::clone(&queue);
            let start = Arc::clone(&start);
            let done = Arc::clone(&done);
            handles.push(thread::spawn(move || {
                start.wait();
                for i in 0..n_per_producer {
                    let value = thread_id * n_per_producer + i;
                    queue.push(value);
                }
                done.wait();
            }));
        }

        start.wait();
        let mut popped = vec![0usize; n_producers];
        let mut count = 0;
        while count < total {
            if let Some(value) = queue.pop() {
                let thread_id = value / n_per_producer;
                assert!(thread_id < n_producers, "unexpected thread_id {thread_id}");
                assert!(
                    popped[thread_id] < n_per_producer,
                    "duplicate from thread {thread_id}"
                );
                popped[thread_id] += 1;
                count += 1;
            } else {
                thread::yield_now();
            }
        }
        done.wait();

        for handle in handles {
            handle.join().unwrap();
        }

        for (id, count) in popped.iter().enumerate() {
            assert_eq!(*count, n_per_producer, "thread {id} missing items");
        }

        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn test_overflow() {
        let capacity = 8;
        let total = 128;
        let queue = Arc::new(ExperimentalMpscQueue::<usize>::new(capacity));
        let producer_count = 4;
        let barrier = Arc::new(Barrier::new(producer_count));

        let mut handles = Vec::new();
        for thread_id in 0..producer_count {
            let queue = Arc::clone(&queue);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                for i in 0..total / producer_count {
                    let value = thread_id * (total / producer_count) + i;
                    queue.push(value);
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        assert!(queue.dropped() > 0, "overflow should have caused drops");
    }
}
