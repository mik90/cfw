use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::vec::Vec;

pub struct ArenaPtr<T> {
    /// Holds a given slot in the arena
    ptr: NonNull<ArenaSlot<T>>,
}

impl<T> ArenaPtr<T> {
    fn slot(&self) -> &ArenaSlot<T> {
        // SAFETY: the arena should always keep these alive, and pub-sub connections will be destroyed before
        // the arenas go away
        unsafe { self.ptr.as_ref() }
    }
    fn slot_mut(&mut self) -> &mut ArenaSlot<T> {
        // SAFETY: the arena should always keep these alive, and pub-sub connections will be destroyed before
        // the arenas go away
        unsafe { self.ptr.as_mut() }
    }
    // TODO impl non-default try_new which allows you to forward args
}

impl<T: Default> ArenaPtr<T> {
    fn try_new(slot: &ArenaSlot<T>) -> Option<ArenaPtr<T>> {
        // Atomically claim the slot: 0 → 1. Fails if live refs or TOMBSTONE exist.
        // Acquire syncs with the Release store that cleared a previous TOMBSTONE,
        // ensuring a prior T's destructor fully completed before we write a new one.
        slot.ref_count
            .compare_exchange(0, 1, atomic::Ordering::Acquire, atomic::Ordering::Relaxed)
            .ok()?;

        // SAFETY: We have exclusive write access — the CAS guarantees no other thread
        // holds a reference (count was 0) and no destructor is running (TOMBSTONE != 0).
        unsafe {
            (*slot.payload.get()).write(T::default());
        }
        Some(ArenaPtr {
            ptr: NonNull::from_ref(slot),
        })
    }
}

impl<T> Clone for ArenaPtr<T> {
    fn clone(&self) -> Self {
        if self
            .slot()
            .ref_count
            .fetch_add(1, atomic::Ordering::Relaxed)
            > usize::MAX / 2
        {
            panic!("Reached the max amount of ArenaPtrs per process");
        }
        ArenaPtr { ptr: self.ptr }
    }
}

impl<T> Deref for ArenaPtr<T> {
    type Target = ArenaSlot<T>;
    fn deref(&self) -> &Self::Target {
        self.slot()
    }
}

impl<T> DerefMut for ArenaPtr<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.slot_mut()
    }
}

impl<T> Drop for ArenaPtr<T> {
    fn drop(&mut self) {
        let slot = self.slot();
        let mut current = slot.ref_count.load(atomic::Ordering::Relaxed);
        loop {
            if current == 1 {
                // We're the last ArenaPtr. No other ArenaPtr exists so no concurrent
                // clone is possible, and try_new only claims slots at count 0 so it
                // won't race with us here. Destroy before storing 0 so the slot is
                // never visible as available while the destructor is still running.
                atomic::fence(atomic::Ordering::Acquire);
                // SAFETY: count == 1 guarantees exclusive access to the payload.
                unsafe { (*slot.payload.get()).assume_init_drop() }
                slot.ref_count.store(0, atomic::Ordering::Release);
                return;
            }
            // Keep trying to decrease ref count and stick in the loop if we haven't decreased it
            // since we may have hit one if another thread decremented the ref count in parallel.
            match slot.ref_count.compare_exchange_weak(
                current,
                current - 1,
                atomic::Ordering::Release,
                atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }
}

unsafe impl<T: Send + Sync> Send for ArenaPtr<T> {}

/// Each entry is reserved via a mutex
pub struct ArenaSlot<T> {
    pub ref_count: AtomicUsize,
    pub payload: UnsafeCell<MaybeUninit<T>>,
}

impl<T> Default for ArenaSlot<T> {
    fn default() -> Self {
        ArenaSlot {
            ref_count: AtomicUsize::new(0),
            payload: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }
}

pub struct Arena<T> {
    // A vector of slots, where each slot can be updated but each value can be mutated too
    storage: Vec<ArenaSlot<T>>,
}

impl<T> Arena<T> {
    pub fn new(capacity: usize) -> Self {
        Arena {
            storage: Vec::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.storage.capacity()
    }

    /// This will invalidate all ArenaPtrs
    pub fn update_capacity(&mut self, new_capacity: usize) {
        self.storage = Vec::with_capacity(new_capacity)
    }

    /// Once the capacity is set, this allocates slots of uninitialized memory
    pub fn allocate_slots(&mut self) {
        for _ in 0..self.storage.capacity() {
            self.storage.push(ArenaSlot::default());
        }
    }
}

impl<T: Default> Arena<T> {
    pub fn try_allocate_default(&mut self) -> Option<ArenaPtr<T>> {
        for slot in self.storage.iter() {
            match ArenaPtr::try_new(slot) {
                Some(ptr) => {
                    return Some(ptr);
                }
                None => continue,
            }
        }
        None
    }

    pub fn allocate_default(&mut self) -> ArenaPtr<T> {
        match self.try_allocate_default() {
            Some(v) => v,
            None => {
                let slot_count = self.storage.len();
                panic!(
                    "The pub-sub system should avoid going beyond allocation capacity. Used {} slots out of capacity of {}",
                    slot_count,
                    self.capacity()
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_arena_ptr() {
        let mut slot = ArenaSlot::<u32>::default();

        let maybe_ptr = ArenaPtr::try_new(&mut slot);
        assert!(maybe_ptr.is_some());

        let ptr = maybe_ptr.unwrap();
        // SAFETY: We have exclusive access to the slot since we just made it
        unsafe {
            (*ptr.payload.get()).write(10);
        }
        assert_eq!(ptr.ref_count.load(atomic::Ordering::Relaxed), 1);
    }

    // Demonstrates the drop/try_new race: if an ArenaPtr is dropped on thread A
    // while thread B (the arena owner) calls try_allocate_default concurrently,
    // try_new can see ref_count == 0 and begin writing T::default() to the slot
    // while thread A's destructor is still running. Run with MIRI or ThreadSanitizer
    // to observe this as a reported data race.
    #[test]
    fn test_drop_reuse_race() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::thread;

        // Statics rather than locals: Drop::drop only receives &mut self and cannot
        // close over variables from the enclosing test function, so the signals must
        // live somewhere the impl block can name directly.
        static DROP_STARTED: AtomicBool = AtomicBool::new(false);
        static DROP_CAN_FINISH: AtomicBool = AtomicBool::new(false);
        // In case we re-run in the same-process, reset
        DROP_STARTED.store(false, Ordering::Release);
        DROP_CAN_FINISH.store(false, Ordering::Release);

        struct SlowDrop {
            value: u64,
        }

        impl Default for SlowDrop {
            fn default() -> Self {
                SlowDrop { value: 0xDEAD_BEEF }
            }
        }

        impl Drop for SlowDrop {
            fn drop(&mut self) {
                // Pause here so the race window stays open after ref_count hits 0.
                DROP_STARTED.store(true, Ordering::Release);
                while !DROP_CAN_FINISH.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                // Reading self.value here races with try_new's write of T::default()
                // to the same memory — TSAN/MIRI will flag this.
                let _ = std::hint::black_box(self.value);
            }
        }

        let mut arena: Arena<SlowDrop> = Arena::new(1);
        arena.allocate_slots();
        let ptr = arena.try_allocate_default().unwrap();

        // Thread A: drop the last ArenaPtr. Its destructor will signal and spin.
        let handle = thread::spawn(move || {
            drop(ptr);
        });

        // Wait until the tombstone is set but assume_init_drop hasn't finished.
        while !DROP_STARTED.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }

        // try_new only claims when count == 0. While SlowDrop::drop runs, count is
        // still 1 (we store 0 only after assume_init_drop returns), so try_new skips it.
        let second_ptr = arena.try_allocate_default();

        DROP_CAN_FINISH.store(true, Ordering::Release);
        handle.join().unwrap();

        assert!(
            second_ptr.is_none(),
            "try_new should have seen count == 1 (not 0) and skipped the slot"
        );
    }

    #[test]
    fn test_arena_allocation() {
        let mut arena: Arena<u32> = Arena::new(2);
        arena.allocate_slots();
        assert_eq!(arena.storage.len(), 2);

        let maybe_ptr1 = arena.try_allocate_default();
        assert!(maybe_ptr1.is_some());
        let ptr1 = maybe_ptr1.unwrap();

        let maybe_ptr2 = arena.try_allocate_default();
        assert!(maybe_ptr2.is_some());
        let ptr2 = maybe_ptr2.unwrap();

        assert_eq!(ptr1.ref_count.load(atomic::Ordering::Relaxed), 1);
        assert_eq!(ptr2.ref_count.load(atomic::Ordering::Relaxed), 1);
        // SAFETY: We have exclusive access to the slot since we just made it
        unsafe {
            (*ptr1.payload.get()).write(1);
            (*ptr2.payload.get()).write(2);
        }

        {
            let ptr1_clone = ptr1.clone();
            assert_eq!(ptr1.ref_count.load(atomic::Ordering::Relaxed), 2);
            drop(ptr1);
            assert_eq!(ptr1_clone.ref_count.load(atomic::Ordering::Relaxed), 1);

            unsafe {
                assert_eq!((*ptr1_clone.payload.get()).assume_init_read(), 1);
                assert_eq!((*ptr2.payload.get()).assume_init_read(), 2);
            }
        }
    }
}
