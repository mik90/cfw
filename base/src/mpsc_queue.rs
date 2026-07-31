use crossbeam_queue::ArrayQueue;
use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::{
    ops::{Deref, DerefMut},
    sync::atomic::{
        AtomicBool, AtomicU8, AtomicUsize,
        Ordering::{self, Acquire, Relaxed, Release},
    },
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
    read_ticket: CacheAligned<usize>,
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
            read_ticket: 0.into(),
            capacity,
            slots: vec,
        }
    }

    pub fn push(&self, value: T) -> bool {
        let new_write_ticket = self.write_ticket.fetch_add(1, Relaxed);
        let turn = (new_write_ticket / self.capacity) as u8;

        let cur_slot = &self.slots[new_write_ticket % self.capacity];
        // Check if it's not our turn and if we should drop
        // serialize with reader
        if turn != cur_slot.turn.load(Acquire) {
            self.dropped.fetch_add(1, Relaxed);
            return false;
        }

        cur_slot.full.store(true, Release); // serialize with reader
        // We can assume that this value was not initialized since pop should de-init entries
        *cur_slot.value.get_mut() = MaybeUninit::new(value);

        true
    }

    pub fn dropped(&self) -> usize {
        self.dropped.load(Relaxed)
    }

    pub fn pop(&mut self) -> Option<T> {
        let read_ticket = *self.read_ticket;

        let turn = (read_ticket / self.capacity) as u8;

        let cur_slot = &mut self.slots[read_ticket % self.capacity];
        if !cur_slot.full.load(Acquire) {
            // nothing left
            return None;
        }
        // TODO could just make read ticket atomic even though i dont have a way to
        // avoid exposing reader interface to multiple threads
        *self.read_ticket += 1;
        let mut return_value = MaybeUninit::uninit();

        // SAFETY: Both the cur slot and return value are the same type/alignment
        unsafe {
            // The return value now has the data from the current slot.
            // So the current slot is uninitialized memory.
            std::ptr::swap(cur_slot.value.as_mut_ptr(), return_value.as_mut_ptr());
        }

        cur_slot.full.store(false, Release);
        cur_slot.turn.store(turn + 1, Release); // serialize w/ writer
        // SAFETY: We initialize values in push(), and we just swapped with an initialized value
        unsafe { Some(return_value.assume_init()) }
    }

    pub fn is_empty(&self) -> bool {
        self.slots
            .iter()
            .all(|slot| slot.full.load(Relaxed) == false)
    }

    pub fn clear(&mut self) {
        while let Some(_) = self.pop() {}
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

    use crate::mpsc_queue::MpscQueue2;

    #[test]
    fn test_push_pop() {
        let mut queue = MpscQueue2::<usize>::new(2);
        assert!(queue.push(1));
        assert_matches!(queue.pop(), Some(1));
    }
}
