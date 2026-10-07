use crossbeam_queue::ArrayQueue;
use std::cell::UnsafeCell;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::ptr::NonNull;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::vec::Vec;

pub struct ArenaPtr<'arena, T> {
    /// Holds a given slot in the arena with pre-initialized data.
    ptr: NonNull<ArenaSlot<T>>,

    /// Freelist owned by the arena
    free_list: &'arena ArrayQueue<usize>,
}

impl<'arena, T> ArenaPtr<'arena, T> {
    fn slot(&self) -> &ArenaSlot<T> {
        // SAFETY: the arena should always keep these alive, and pub-sub connections will be destroyed before
        // the arenas go away
        unsafe { self.ptr.as_ref() }
    }
    // TODO impl non-default try_new which allows you to forward args
}

impl<'arena, T: Default> ArenaPtr<'arena, T> {
    fn try_new(
        slot: &ArenaSlot<T>,
        free_list: &'arena ArrayQueue<usize>,
    ) -> Option<ArenaPtr<'arena, T>> {
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
            free_list,
        })
    }
}

impl<'arena, T> Clone for ArenaPtr<'arena, T> {
    fn clone(&self) -> Self {
        if self
            .slot()
            .ref_count
            .fetch_add(1, atomic::Ordering::Relaxed)
            > usize::MAX / 2
        {
            panic!("Reached the max amount of ArenaPtrs per process");
        }
        ArenaPtr {
            ptr: self.ptr,
            free_list: self.free_list,
        }
    }
}

impl<'arena, T> Deref for ArenaPtr<'arena, T> {
    type Target = ArenaSlot<T>;
    fn deref(&self) -> &Self::Target {
        self.slot()
    }
}

impl<'arena, T> Drop for ArenaPtr<'arena, T> {
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

/// SAFETY: `Send` permits ownership and the final drop to occur on another
/// thread; `Sync` is required because cloned pointers permit concurrent
/// immutable reads of the same published value.
unsafe impl<'arena, T: Send + Sync> Send for ArenaPtr<'arena, T> {}

/// An exclusive writer-side reservation. Dropping it releases the slot without
/// dropping its possibly partially initialized payload.
pub struct ArenaPtrUninit<'arena, T> {
    /// Holds a given slot in the arena, although the memory isn't initialized yet.
    ptr: NonNull<ArenaSlot<T>>,

    /// Freelist owned by the arena
    free_list: &'arena ArrayQueue<usize>,
}

impl<'arena, T> ArenaPtrUninit<'arena, T> {
    fn try_new(slot: &ArenaSlot<T>, free_list: &'arena ArrayQueue<usize>) -> Option<Self> {
        // Claim exclusive ownership: 0 → 1. Acquire pairs with the previous
        // owner's Release when freeing the slot, including completion of any drop.
        slot.ref_count
            .compare_exchange(0, 1, atomic::Ordering::Acquire, atomic::Ordering::Relaxed)
            .ok()?;
        Some(Self {
            ptr: NonNull::from_ref(slot),
            free_list,
        })
    }

    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        // SAFETY: The arena keeps this slot alive and aligned. Retain shared
        // access to the slot; mutable access is confined to its UnsafeCell.
        let slot = unsafe { self.ptr.as_ref() };
        // SAFETY: This unpublished loan exclusively owns the payload, and
        // &mut self ties the returned borrow to exclusive access to the loan.
        unsafe { &mut *slot.payload.get() }
    }

    /// # Safety
    ///
    /// Ensure that the payload is fully initialized before calling this
    pub unsafe fn assume_init(self) -> ArenaPtr<'arena, T> {
        let free_list = self.free_list;
        let this = ManuallyDrop::new(self);
        ArenaPtr {
            ptr: this.ptr,
            free_list,
        }
    }
}

impl<'arena, T> Drop for ArenaPtrUninit<'arena, T> {
    fn drop(&mut self) {
        // SAFETY: The arena keeps the slot alive, and this unpublished loan is
        // its sole owner since uninitialized arena pointers are only usable on the writer side.
        // The payload may be partially initialized, so do not drop T.
        let slot = unsafe { self.ptr.as_ref() };
        slot.ref_count.store(0, atomic::Ordering::Release);
    }
}

/// Pointer to a message that we assume is read-only based on pub/sub invariants
#[derive(Clone)]
pub struct ArenaReaderPtr<'arena, T> {
    /// Holds a normal ArenaPtr, just marked as read-only
    ptr: ArenaPtr<'arena, T>,
}

impl<'arena, T> ArenaReaderPtr<'arena, T> {
    /// This should only be created on already-published ptrs
    pub fn new(ptr: ArenaPtr<'arena, T>) -> Self {
        Self { ptr }
    }
}

/// This should only be created on already-published ptrs
impl<'arena, T> From<ArenaPtr<'arena, T>> for ArenaReaderPtr<'arena, T> {
    fn from(ptr: ArenaPtr<'arena, T>) -> Self {
        Self { ptr }
    }
}

impl<'arena, T> Deref for ArenaReaderPtr<'arena, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        // SAFETY: The caller should have ensured that this ptr is on a message already published in the pub/sub system.
        unsafe { (*self.ptr.payload.get()).assume_init_ref() }
    }
}

/// A slot in the arena. Read/write access is controlled by the pub/sub framework.
pub struct ArenaSlot<T> {
    /// How many references are alive, regardless of whether a publisher or subscriber owns a given slot
    ref_count: AtomicUsize,
    /// Managed data. UnsafeCell gets around the limitation of how the compiler doesn't know that the pub/sub framework
    /// avoids mulitple writers on a given slot.
    /// Since we re-use slots, the data maybe uninitialized.
    pub payload: UnsafeCell<MaybeUninit<T>>,
}

impl<T> ArenaSlot<T> {
    /// # Safety
    ///
    /// Caller should ensure that this slot has already been initialized
    pub unsafe fn assume_init_ref(&self) -> &T {
        debug_assert!(self.ref_count.load(atomic::Ordering::Acquire) > 0);

        // SAFETY: It is up to caller to manage invariant of pre-initialized arena slot
        unsafe { (*self.payload.get()).assume_init_ref() }
    }
}

pub struct Arena<T> {
    capacity: usize,
    // A vector of slots, where each slot can be updated but each value can be mutated too
    storage: Box<[ArenaSlot<T>]>,
    /// MPSC queue (although it can handle MPMC) where pub/sub publishers consume entires off the freelist to publish
    /// data, and subscribers produce freelist entries as they become done with slots. Publishers can also return slots
    /// although they have exclusive ownership at that point.
    free_list: ArrayQueue<usize>,
}

impl<T> Arena<T> {
    /// Sets initial capacity, although these pointers may be cleared out once slots are re-allocated
    pub fn new(capacity: usize) -> Self {
        let mut arena = Arena {
            // Basically empty storage until we call allocate_slots()
            storage: vec![].into_boxed_slice(),
            capacity,
            free_list: ArrayQueue::new(capacity),
        };
        arena.reallocate_slots();
        arena
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// This will invalidate all ArenaPtrs
    pub fn update_capacity(&mut self, new_capacity: usize) {
        self.capacity = new_capacity;
    }

    /// This runs over all the storage and populates the freelist.
    /// Only meant to be run on construction or allocation, before arena is actually uysed.
    fn populate_free_list(&mut self) {}

    /// Once the capacity is set, this allocates slots of uninitialized memory
    pub fn reallocate_slots<'arena>(&'arena mut self) {
        let mut vec_storage: Vec<ArenaSlot<T>> = Vec::with_capacity(self.capacity);

        // Initialize each element
        for _ in 0..vec_storage.capacity() {
            // SAFETY: We will initialize members of ArenaSlot that need to be initialized, so just the ref count
            unsafe {
                let vec_ptr = vec_storage.as_mut_ptr().add(vec_storage.len());
                let uninit_ref_count = &raw mut (*vec_ptr).ref_count;
                uninit_ref_count.write(AtomicUsize::new(0));
            }
            // SAFETY: We've just initialized a new entry
            unsafe {
                vec_storage.set_len(vec_storage.len() + 1);
            }
        }
        self.storage = vec_storage.into_boxed_slice();

        self.free_list = ArrayQueue::new(self.capacity);
        for (index, slot) in self.storage.iter().enumerate() {
            if slot.ref_count.load(atomic::Ordering::Relaxed) == 0 {
                self.free_list.force_push(index);
            }
        }
    }
}

impl<T> Arena<T> {
    /// Allocates a slot without initializing memory
    pub fn try_allocate_uninit<'arena>(&'arena self) -> Option<ArenaPtrUninit<'arena, T>> {
        self.storage
            .iter()
            .find_map(|slot| ArenaPtrUninit::try_new(slot, &self.free_list))
    }

    pub fn allocate_uninit<'arena>(&'arena self) -> ArenaPtrUninit<'arena, T> {
        match self.try_allocate_uninit() {
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

    pub fn try_allocate_with<'arena>(
        &'arena self,
        factory: impl FnOnce(&mut MaybeUninit<T>),
    ) -> Option<ArenaPtr<'arena, T>> {
        let mut uninit_ptr = self.try_allocate_uninit()?;
        factory(uninit_ptr.payload_uninit());
        // SAFETY: The factory is responsible for fully initializing the payload.
        Some(unsafe { uninit_ptr.assume_init() })
    }
}

#[cfg(test)]
mod tests {

    impl<T: Default> Arena<T> {
        pub fn try_allocate_default<'arena>(&'arena self) -> Option<ArenaPtr<'arena, T>> {
            self.try_allocate_with(|slot| {
                slot.write(T::default());
            })
        }
    }
    use super::*;

    struct DropCounter<'a>(&'a AtomicUsize);

    impl Drop for DropCounter<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn uninit_drop_releases_slot_without_dropping_payload() {
        let drops = AtomicUsize::new(0);
        let mut arena = Arena::new(1);
        arena.reallocate_slots();
        let mut loan = arena.try_allocate_uninit().unwrap();
        assert_eq!(
            arena.storage[0].ref_count.load(atomic::Ordering::Relaxed),
            1
        );
        assert!(arena.try_allocate_uninit().is_none());
        loan.payload_uninit().write(DropCounter(&drops));
        drop(loan);

        assert_eq!(drops.load(atomic::Ordering::Relaxed), 0);
        assert!(arena.try_allocate_uninit().is_some());
    }

    #[test]
    fn uninit_assume_init_transfers_ownership() {
        let drops = AtomicUsize::new(0);
        let mut arena = Arena::new(1);
        arena.reallocate_slots();
        let mut loan = arena.try_allocate_uninit().unwrap();
        loan.payload_uninit().write(DropCounter(&drops));
        // SAFETY: The payload was fully initialized above.
        let initialized = unsafe { loan.assume_init() };
        assert!(arena.try_allocate_uninit().is_none());
        assert_eq!(drops.load(atomic::Ordering::Relaxed), 0);
        drop(initialized);

        assert_eq!(drops.load(atomic::Ordering::Relaxed), 1);
        assert!(arena.try_allocate_uninit().is_some());
    }

    #[test]
    fn uninit_factory_panic_releases_slot() {
        let mut arena = Arena::<u64>::new(1);
        arena.reallocate_slots();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            arena.try_allocate_with(|_| panic!("Factory failed"));
        }));
        assert!(result.is_err());

        let initialized = arena
            .try_allocate_with(|slot| {
                slot.write(42);
            })
            .unwrap();
        assert!(arena.try_allocate_uninit().is_none());
        assert_eq!(*ArenaReaderPtr::new(initialized), 42);
        assert!(arena.try_allocate_uninit().is_some());
    }

    #[test]
    fn test_arena_ptr() {
        let slot: ArenaSlot<i32> = ArenaSlot {
            ref_count: AtomicUsize::new(0),
            payload: UnsafeCell::new(MaybeUninit::uninit()),
        };
        let free_list = ArrayQueue::new(1);
        free_list.force_push(0);

        let maybe_ptr = ArenaPtr::try_new(&slot, &free_list);
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
        arena.reallocate_slots();
        let ptr = arena.try_allocate_default().unwrap();

        // Thread A: drop the last ArenaPtr. Its destructor will signal and spin.
        thread::scope(|scope| {
            let thread_handle = scope.spawn(move || {
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
            thread_handle.join().unwrap();

            assert!(
                second_ptr.is_none(),
                "try_new should have seen count == 1 (not 0) and skipped the slot"
            );
        });
    }

    #[test]
    fn test_arena_allocation() {
        let mut arena: Arena<u32> = Arena::new(2);
        arena.reallocate_slots();
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

            // SAFETY: This is a unit test, and we did init the payload before cloning
            unsafe {
                assert_eq!((*ptr1_clone.payload.get()).assume_init_read(), 1);
                assert_eq!((*ptr2.payload.get()).assume_init_read(), 2);
            }
        }
    }

    /// Ensures that we won't overflow the stack if we have a large type in our arena
    #[test]
    fn test_large_type_handling() {
        const LARGE_SIZE: usize = 11_000_000;

        const ONE_MB_BYTES: usize = 1_000_000;
        const DEFAULT_STACK_HEIGHT_BYTES: usize = ONE_MB_BYTES * 11;

        // Should not be able to fit into stack
        pub struct LargeMessage {
            big_array: [u64; LARGE_SIZE],
        }
        const {
            if std::mem::size_of::<LargeMessage>() < DEFAULT_STACK_HEIGHT_BYTES {
                panic!("LargeMessage isn't large enough");
            }
        }

        const CAPACITY: usize = 100;
        let mut arena = Arena::<LargeMessage>::new(CAPACITY);
        arena.reallocate_slots();
        for index in 0..CAPACITY {
            let allocate_result = arena.try_allocate_uninit();
            assert!(
                allocate_result.is_some(),
                "Could not allocate the entry index {}",
                index
            );
        }
    }
}
