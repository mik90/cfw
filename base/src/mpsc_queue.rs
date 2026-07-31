use crossbeam_queue::ArrayQueue;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicUsize,
    Ordering::{self, Acquire, Relaxed, Release},
};

/// A bounded, lock-free MPSC queue backed by `crossbeam_queue::ArrayQueue`.
/// When the queue is full, `push` displaces the oldest element (front) to make room.
///
/// The internals can be replaced with a hand-rolled implementation without
/// changing any call sites — the public API is the only contract.
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
        while self.pop().is_some() {
            // Keep popping while there are inputs
        }
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

// Default to 64, could differ per platform
#[repr(align(64))]
#[derive(Debug)]
struct CacheAligned<T> {
    value: T,
}

impl<T> CacheAligned<T> {
    pub fn new(value: T) -> Self {
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

struct QueueSlot<T> {
    pub turn: CacheAligned<AtomicU8>,
    pub full: CacheAligned<AtomicBool>,
    pub value: UnsafeCell<MaybeUninit<T>>,
}

///  https://blog.bearcats.nl/simple-message-queue/ (modified rigtorp?) to replace cross-beam channel usage which is MPMC
pub struct MpscQueue2<T> {
    /// Cumulative count of elements displaced by `push` due to overflow.
    dropped: CacheAligned<AtomicUsize>,
    write_ticket: CacheAligned<AtomicUsize>,
    /// Read ticket doesn't need to be atomic, there's a single reader
    read_ticket: CacheAligned<UnsafeCell<usize>>,
    capacity: usize,
    slots: Vec<QueueSlot<T>>,
}

impl<T> MpscQueue2<T> {
    pub fn new(capacity: usize) -> Self {
        let mut vec = Vec::with_capacity(capacity);
        vec.resize_with(capacity, || QueueSlot {
            turn: AtomicU8::new(0).into(),
            full: AtomicBool::new(false).into(),
            value: UnsafeCell::new(MaybeUninit::<T>::uninit()),
        });
        Self {
            dropped: AtomicUsize::new(0).into(),
            write_ticket: AtomicUsize::new(0).into(),
            read_ticket: UnsafeCell::new(0).into(),
            capacity,
            slots: vec,
        }
    }

    pub fn push(&self, value: T) -> bool {
        loop {
            let ticket = self.write_ticket.load(Relaxed);
            let slot = &self.slots[ticket % self.capacity];
            let turn = (ticket / self.capacity) as u8;

            // Slot not yet consumed by reader for this turn — can't write.
            if turn != slot.turn.load(Acquire) {
                self.dropped.fetch_add(1, Relaxed);
                return false;
            }

            // Try to claim this ticket. If CAS fails, another producer
            // already claimed it — retry with the new ticket.
            if self
                .write_ticket
                .compare_exchange_weak(ticket, ticket + 1, Acquire, Relaxed)
                .is_ok()
            {
                // SAFETY: Claimed exclusive access via successful CAS.
                // The slot's previous value was left uninitialized by pop().
                unsafe {
                    *slot.value.get() = MaybeUninit::new(value);
                }
                slot.full.store(true, Release); // publish to reader
                return true;
            }
        }
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Relaxed)
    }

    pub fn pop(&self) -> Option<T> {
        // SAFETY: Single consumer — only one thread ever calls pop().
        let read_ticket = unsafe { *self.read_ticket.get() };

        let turn = (read_ticket / self.capacity) as u8;

        let cur_slot = &self.slots[read_ticket % self.capacity];
        if !cur_slot.full.load(Acquire) {
            // nothing left
            return None;
        }
        // SAFETY: Single consumer — only one thread ever calls pop().
        unsafe {
            *self.read_ticket.get() = read_ticket + 1;
        }
        let mut return_value = MaybeUninit::uninit();

        // SAFETY: Both the cur slot and return value are the same type/alignment.
        // The return value now has the data from the current slot.
        // So the current slot is uninitialized memory.
        unsafe {
            std::ptr::swap(
                (*cur_slot.value.get()).as_mut_ptr(),
                return_value.as_mut_ptr(),
            );
        }

        cur_slot.full.store(false, Release);
        cur_slot.turn.store(turn + 1, Release); // serialize w/ writer
        // SAFETY: We initialize values in push(), and we just swapped with an initialized value
        unsafe { Some(return_value.assume_init()) }
    }

    pub fn is_empty(&self) -> bool {
        self.slots.iter().all(|slot| !slot.full.load(Relaxed))
    }

    pub fn clear(&self) {
        while self.pop().is_some() {}
    }

    pub fn len(&self) -> usize {
        let mut count = 0;
        for entry in self.slots.iter() {
            if entry.full.load(Relaxed) {
                count += 1;
            }
        }
        count
    }
}

// SAFETY: The ticket/turn algorithm serializes writers so only one thread
// has access to a given slot's value at a time, and the reader synchronizes
// via Acquire/Release on the `full` flag. T: Send ensures queued values
// can safely move between threads.
unsafe impl<T: Send> Sync for MpscQueue2<T> {}

impl<T> Drop for MpscQueue2<T> {
    fn drop(&mut self) {
        for slot in self.slots.iter_mut() {
            if slot.full.load(Acquire) {
                // SAFETY: The value must've been initialized if it is full
                unsafe {
                    slot.value.get_mut().assume_init_drop();
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use std::assert_matches;
    use std::sync::{Arc, Barrier};
    use std::thread;

    use crate::mpsc_queue::MpscQueue2;

    #[test]
    fn test_push_pop() {
        let queue = MpscQueue2::<usize>::new(2);
        assert!(queue.push(1));
        assert_matches!(queue.pop(), Some(1));
    }

    #[test]
    fn test_single_producer_single_consumer() {
        let n = 1_000;
        let queue = MpscQueue2::<usize>::new(64);

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
        let n_per_producer = 1_000;
        let total = n_producers * n_per_producer;
        let queue = Arc::new(MpscQueue2::<usize>::new(64));
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
                    while !queue.push(value) {
                        thread::yield_now();
                    }
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
        let queue = Arc::new(MpscQueue2::<usize>::new(capacity));
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
