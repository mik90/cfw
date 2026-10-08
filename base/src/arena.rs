use crossbeam_queue::ArrayQueue;
use std::cell::UnsafeCell;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ops::Deref;
use std::sync::atomic;
use std::sync::atomic::AtomicUsize;
use std::vec::Vec;

pub struct ArenaPtr<'arena, T> {
    /// Holds a given slot in the arena with pre-initialized data.
    ptr: &'arena ArenaSlot<T>,
    index: usize,

    /// Freelist owned by the arena
    free_list: &'arena ArrayQueue<usize>,
}

impl<'arena, T> ArenaPtr<'arena, T> {
    fn slot(&self) -> &ArenaSlot<T> {
        self.ptr
    }
    // TODO impl non-default try_new which allows you to forward args
}

#[cfg(test)]
impl<'arena, T: Default> ArenaPtr<'arena, T> {
    fn try_new(
        storage: &'arena [ArenaSlot<T>],
        free_list: &'arena ArrayQueue<usize>,
    ) -> Option<ArenaPtr<'arena, T>> {
        let mut loan = ArenaPtrUninit::try_new(storage, free_list)?;
        loan.payload_uninit().write(T::default());
        // SAFETY: The default value fully initialized the payload.
        Some(unsafe { loan.assume_init() })
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
            index: self.index,
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
                // clone is possible. Destroy before returning the index so the slot
                // is never available while the destructor is still running.
                atomic::fence(atomic::Ordering::Acquire);
                // SAFETY: count == 1 guarantees exclusive access to the payload.
                unsafe { (*slot.payload.get()).assume_init_drop() }
                slot.ref_count.store(0, atomic::Ordering::Release);
                self.free_list
                    .push(self.index)
                    .expect("slot returned twice");
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
    ptr: &'arena ArenaSlot<T>,
    index: usize,

    /// Freelist owned by the arena
    free_list: &'arena ArrayQueue<usize>,
}

impl<'arena, T> ArenaPtrUninit<'arena, T> {
    fn try_new(
        storage: &'arena [ArenaSlot<T>],
        free_list: &'arena ArrayQueue<usize>,
    ) -> Option<Self> {
        let index = free_list.pop()?;
        let slot = &storage[index];
        // A queued index exclusively reserves a free slot. Acquire also pairs
        // with the previous owner's Release after its destructor completed.
        assert_eq!(slot.ref_count.swap(1, atomic::Ordering::Acquire), 0);
        Some(Self {
            ptr: slot,
            index,
            free_list,
        })
    }

    pub fn payload_uninit(&mut self) -> &mut MaybeUninit<T> {
        // The arena keeps this slot alive and aligned. Retain shared
        // access to the slot; mutable access is confined to its UnsafeCell.
        let slot = self.ptr;
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
            index: this.index,
            free_list,
        }
    }
}

impl<'arena, T> Drop for ArenaPtrUninit<'arena, T> {
    fn drop(&mut self) {
        // The arena keeps the slot alive, and this unpublished loan is
        // its sole owner since uninitialized arena pointers are only usable on the writer side.
        // The payload may be partially initialized, so do not drop T.
        let slot = self.ptr;
        slot.ref_count.store(0, atomic::Ordering::Release);
        self.free_list
            .push(self.index)
            .expect("slot returned twice");
    }
}

/// Pointer to a message that we assume is read-only based on pub/sub invariants
pub struct ArenaReaderPtr<'arena, T> {
    /// Holds a normal ArenaPtr, just marked as read-only
    ptr: ArenaPtr<'arena, T>,
}
impl<T> Clone for ArenaReaderPtr<'_, T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr.clone(),
        }
    }
}

// SAFETY: This wrapper exposes only immutable payload references. Its inner
// pointer is private, and concurrent clones/drops synchronize via the slot's
// atomic ref count. Sync permits shared reads; Send permits eventual destruction
// on a different thread, including when this reader is a forwarded payload.
unsafe impl<T: Send + Sync> Sync for ArenaReaderPtr<'_, T> {}

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

/// Owns storage borrowed by allocated messages.
///
/// A message cannot outlive its arena:
/// ```compile_fail
/// use base::arena::Arena;
/// let message = {
///     let arena = Arena::<u64>::new(1);
///     arena.allocate_uninit()
/// };
/// drop(message);
/// ```
/// Storage cannot be reallocated while a message still borrows it:
/// ```compile_fail
/// use base::arena::Arena;
/// let mut arena = Arena::<u64>::new(1);
/// let message = arena.allocate_uninit();
/// arena.reallocate_slots();
/// drop(message);
/// ```
pub struct Arena<T> {
    capacity: usize,
    // A vector of slots, where each slot can be updated but each value can be mutated too
    storage: Box<[ArenaSlot<T>]>,
    /// MPSC queue (although it can handle MPMC) where pub/sub publishers consume entires off the freelist to publish
    /// data, and subscribers produce freelist entries as they become done with slots. Publishers can also return slots
    /// although they have exclusive ownership at that point.
    free_list: ArrayQueue<usize>,
}

/// A movable allocation capability borrowing fixed arena storage.
///
/// This handle exposes reservations, not direct access to the arena's slots or
/// permission to reallocate storage. Each reservation exclusively owns a slot.
pub struct ArenaAllocator<'arena, T> {
    arena: &'arena Arena<T>,
}

impl<'arena, T> ArenaAllocator<'arena, T> {
    pub fn try_allocate_uninit(&self) -> Option<ArenaPtrUninit<'arena, T>> {
        self.arena.try_allocate_uninit()
    }

    pub fn capacity(&self) -> usize {
        self.arena.capacity()
    }
}

// SAFETY: The borrowed arena cannot move or reallocate while this handle lives.
// Allocation exclusively claims an index from the synchronized free list before
// touching its payload. Final drops return indices only after destruction.
// No shared payload access is exposed by this handle. Send + Sync bounds match
// ArenaPtr's cross-thread ownership and shared-reader requirements.
unsafe impl<T: Send + Sync> Send for ArenaAllocator<'_, T> {}

impl<T> Arena<T> {
    pub fn allocator(&self) -> ArenaAllocator<'_, T> {
        ArenaAllocator { arena: self }
    }

    /// Sets initial capacity, although these pointers may be cleared out once slots are re-allocated
    pub fn new(capacity: usize) -> Self {
        let mut arena = Arena {
            // Basically empty storage until we call allocate_slots()
            storage: vec![].into_boxed_slice(),
            capacity,
            free_list: ArrayQueue::new(capacity.max(1)),
        };
        arena.reallocate_slots();
        arena
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Sets the capacity used by the next reallocation. Requires exclusive access.
    pub fn update_capacity(&mut self, new_capacity: usize) {
        self.capacity = new_capacity;
    }

    /// Once the capacity is set, this allocates slots of uninitialized memory
    pub fn reallocate_slots(&mut self) {
        let mut vec_storage: Vec<ArenaSlot<T>> = Vec::with_capacity(self.capacity);

        // Initialize each element
        for _ in 0..self.capacity {
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

        self.free_list = ArrayQueue::new(self.capacity.max(1));
        for index in 0..self.storage.len() {
            self.free_list
                .push(index)
                .expect("free list capacity matches storage");
        }
    }
}

impl<T> Arena<T> {
    /// Allocates a slot without initializing memory
    pub fn try_allocate_uninit<'arena>(&'arena self) -> Option<ArenaPtrUninit<'arena, T>> {
        ArenaPtrUninit::try_new(&self.storage, &self.free_list)
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

    /// Reserves a slot and initializes its payload in place.
    ///
    /// # Safety
    /// The factory must fully initialize the payload before returning normally.
    /// If it panics, the reservation is released without dropping the payload.
    pub unsafe fn try_allocate_with<'arena>(
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
            ArenaPtr::try_new(&self.storage, &self.free_list)
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
            // SAFETY: This factory never returns normally.
            unsafe { arena.try_allocate_with(|_| panic!("Factory failed")) };
        }));
        assert!(result.is_err());

        // SAFETY: The factory writes a fully initialized u64.
        let initialized = unsafe {
            arena.try_allocate_with(|slot| {
                slot.write(42);
            })
        }
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

        let maybe_ptr = ArenaPtr::try_new(std::slice::from_ref(&slot), &free_list);
        assert!(maybe_ptr.is_some());

        let ptr = maybe_ptr.unwrap();
        // SAFETY: We have exclusive access to the slot since we just made it
        unsafe {
            (*ptr.payload.get()).write(10);
        }
        assert_eq!(ptr.ref_count.load(atomic::Ordering::Relaxed), 1);
    }

    // A slot must remain unavailable until its payload destructor completes.
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
                // Pause while the payload is still being destroyed.
                DROP_STARTED.store(true, Ordering::Release);
                while !DROP_CAN_FINISH.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
                // Reusing this slot prematurely would race with this read.
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

            // Wait until assume_init_drop has started but hasn't finished.
            while !DROP_STARTED.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }

            // The slot's index is not queued until its destructor completes.
            let second_ptr = arena.try_allocate_default();

            DROP_CAN_FINISH.store(true, Ordering::Release);
            thread_handle.join().unwrap();

            assert!(
                second_ptr.is_none(),
                "a slot cannot be reused during payload destruction"
            );
            assert!(arena.try_allocate_default().is_some());
        });
    }

    #[test]
    fn clones_return_each_slot_only_once() {
        let drops = AtomicUsize::new(0);
        let arena = Arena::new(2);
        let mut loan = arena.allocate_uninit();
        loan.payload_uninit().write(DropCounter(&drops));
        // SAFETY: The payload is fully initialized above.
        let ptr = unsafe { loan.assume_init() };
        let clone = ptr.clone();
        let other = arena.allocate_uninit();
        drop(ptr);
        assert!(arena.try_allocate_uninit().is_none());
        assert_eq!(drops.load(atomic::Ordering::Relaxed), 0);
        drop(clone);
        assert_eq!(drops.load(atomic::Ordering::Relaxed), 1);
        let reused = arena.allocate_uninit();
        assert!(arena.try_allocate_uninit().is_none());
        drop(other);
        drop(reused);
        assert_eq!(arena.free_list.len(), 2);
    }

    #[test]
    fn empty_arena_and_reallocation_rebuild_free_list() {
        let mut arena = Arena::<u64>::new(0);
        assert!(arena.try_allocate_uninit().is_none());
        arena.update_capacity(3);
        arena.reallocate_slots();
        let loans: Vec<_> = (0..3).map(|_| arena.allocate_uninit()).collect();
        assert!(arena.try_allocate_uninit().is_none());
        drop(loans);
        arena.update_capacity(1);
        arena.reallocate_slots();
        let loan = arena.allocate_uninit();
        assert!(arena.try_allocate_uninit().is_none());
        drop(loan);
        assert_eq!(arena.free_list.len(), 1);
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
