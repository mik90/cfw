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
        let old_ref_count = slot.ref_count.fetch_add(1, atomic::Ordering::Relaxed);
        if old_ref_count != 0 {
            // this is an already initialized slot, decrement it and abort
            slot.ref_count.fetch_sub(1, atomic::Ordering::Relaxed);
            return None;
        }

        // SAFETY: We have exclusive write access to this slot since the ref count was 0.
        // If someone else tried to get it, they would've early returned due to the ref count.
        unsafe {
            // We have no in-place construction, so we write the default T from the stack
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
        let old_ref_count = self
            .slot()
            .ref_count
            .fetch_sub(1, atomic::Ordering::Release);

        if old_ref_count == 1 {
            atomic::fence(atomic::Ordering::Acquire);
            // SAFETY: Once the counter is 0, nothing has use for this data anymore
            // and we can consider it uninitialized and then dorp it
            unsafe {
                (*self.slot().payload.get()).assume_init_drop();
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

impl<'a, T> Arena<T> {
    pub fn new(capacity: usize) -> Self {
        Arena {
            storage: Vec::with_capacity(capacity),
        }
    }

    pub fn capacity(&self) -> usize {
        self.storage.capacity()
    }

    pub fn clear(&mut self) {
        self.storage.clear();
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

impl<'a, T: Default> Arena<T> {
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
