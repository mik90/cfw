//! Fixed-capacity inline sequence.
use core::mem;
use core::ops::Deref;
use core::slice;

/// Inline sequence of at most `CAPACITY` entries. Never allocates.
pub struct StaticVec<T, const CAPACITY: usize> {
    /// Array of possibly initialized entries
    entries: [mem::MaybeUninit<T>; CAPACITY],
    len: usize,
}

impl<T, const CAPACITY: usize> StaticVec<T, CAPACITY> {
    pub const fn new() -> Self {
        Self {
            // Const block allows this to build for some reason, presumably because
            // it creates a new uninit instead of copying the same uninit repeatedly.
            entries: [const { mem::MaybeUninit::uninit() }; CAPACITY],
            len: 0,
        }
    }

    /// Append `value`, returning false when the sequence is full.
    pub fn push(&mut self, value: T) -> bool {
        if self.len == CAPACITY {
            return false;
        }
        self.len += 1;
        self.entries[self.len - 1] = mem::MaybeUninit::new(value);
        true
    }

    /// Helper that's really meant for constructing vec in a const context
    pub const fn push_const(mut self, value: T) -> Self {
        assert!(self.len < self.capacity(), "StaticVec is at capacity");
        self.entries[self.len] = mem::MaybeUninit::new(value);
        self.len += 1;
        self
    }

    /// Removes element from back
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }

        let back_uninit = mem::replace(&mut self.entries[self.len - 1], mem::MaybeUninit::uninit());
        self.len -= 1;

        // SAFETY: Length ensures that this value is initialized
        Some(unsafe { back_uninit.assume_init() })
    }

    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    pub fn front(&self) -> Option<&T> {
        if self.len == 0 {
            return None;
        }

        let front_uninit = &self.entries[0];

        // SAFETY: Length ensures that this value is initialized
        Some(unsafe { front_uninit.assume_init_ref() })
    }

    pub fn front_mut(&mut self) -> Option<&mut T> {
        if self.len == 0 {
            return None;
        }

        let front_uninit = &mut self.entries[0];

        // SAFETY: Length ensures that this value is initialized
        Some(unsafe { front_uninit.assume_init_mut() })
    }

    pub fn back(&self) -> Option<&T> {
        if self.len == 0 {
            return None;
        }

        let back_uninit = &self.entries[self.len - 1];

        // SAFETY: Length ensures that this value is initialized
        Some(unsafe { back_uninit.assume_init_ref() })
    }

    pub fn back_mut(&mut self) -> Option<&mut T> {
        if self.len == 0 {
            return None;
        }

        let back_uninit = &mut self.entries[self.len - 1];

        // SAFETY: Length ensures that this value is initialized
        Some(unsafe { back_uninit.assume_init_mut() })
    }

    /// Removes element from back
    pub fn pop_into(&mut self, slot: &mut Option<T>) {
        if self.len == 0 {
            // Nothing to write into option
            return;
        }

        let back_uninit = &mut self.entries[self.len - 1];

        // SAFETY: Length ensures that this value is initialized
        unsafe {
            slot.replace(back_uninit.assume_init_read());
        }

        self.len -= 1;
    }

    pub fn clear(&mut self) {
        for elem in self.entries.iter_mut().take(self.len) {
            // SAFETY:  Len is our invariant that tells us how many elements are initialized, and if they're initialized
            // then we must drop them to clear them
            unsafe {
                elem.assume_init_drop();
            };
        }
        self.len = 0;
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub const fn is_full(&self) -> bool {
        self.len == self.capacity()
    }

    /// Pointer as contained type
    pub fn as_ptr(&self) -> *const T {
        let uninit_ptr = self.entries.as_ptr();

        uninit_ptr as *const T
    }

    /// Slice of initialized data
    pub fn as_slice(&self) -> &[T] {
        // SAFETY: Length determines how much data is initialized
        unsafe { slice::from_raw_parts(self.as_ptr(), self.len) }
    }

    /// Pointer as contained type
    pub fn as_mut_ptr(&mut self) -> *mut T {
        let uninit_ptr = self.entries.as_mut_ptr();

        uninit_ptr as *mut T
    }

    /// Slice of initialized data
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: Length determines how much data is initialized
        unsafe { slice::from_raw_parts_mut(self.as_mut_ptr(), self.len) }
    }
}

impl<T: core::fmt::Debug, const CAPACITY: usize> core::fmt::Debug for StaticVec<T, CAPACITY> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticVec")
            .field("CAPACITY", &CAPACITY)
            .field("len", &self.len())
            .field("entries", &self.as_slice())
            .finish()
    }
}

impl<T: core::fmt::Display, const CAPACITY: usize> core::fmt::Display for StaticVec<T, CAPACITY> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "[")?;
        for (i, entry) in self.as_slice().iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}", entry)?;
        }
        write!(f, "]")
    }
}

impl<T: Clone, const CAPACITY: usize> Clone for StaticVec<T, CAPACITY> {
    fn clone(&self) -> Self {
        let mut cloned = Self::new();
        cloned.len = self.len;

        // The len invariants enforces that these entries are already initialized
        // so we can safely know how to clone them.
        let initialized_entries = self.as_slice().iter();

        for (i, entry) in initialized_entries.enumerate() {
            cloned.entries[i] = mem::MaybeUninit::new(entry.clone());
        }

        cloned
    }
}

impl<T: Clone, const CAPACITY: usize> StaticVec<T, CAPACITY> {
    pub fn push_cloned(&mut self, value: &T) -> bool {
        if self.len == CAPACITY {
            return false;
        }
        self.entries[self.len - 1] = mem::MaybeUninit::new(value.clone());
        self.len += 1;
        true
    }
}

impl<T: PartialEq, const CAPACITY: usize> PartialEq for StaticVec<T, CAPACITY> {
    fn eq(&self, other: &Self) -> bool {
        let initialized_iter = self.as_slice().iter();

        let other_initialized_iter = other.as_slice().iter();

        initialized_iter.eq(other_initialized_iter)
    }
}

impl<T, const CAPACITY: usize> Drop for StaticVec<T, CAPACITY> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T: Copy + Default, const CAPACITY: usize> Default for StaticVec<T, CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default, const CAPACITY: usize> Deref for StaticVec<T, CAPACITY> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

#[cfg(feature = "serde")]
impl<T: Copy + Default + serde::Serialize, const CAPACITY: usize> serde::Serialize
    for StaticVec<T, CAPACITY>
{
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.as_slice(), serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T, const CAPACITY: usize> serde::Deserialize<'de> for StaticVec<T, CAPACITY>
where
    T: Copy + Default + serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T, const CAPACITY: usize>(core::marker::PhantomData<T>);
        impl<'de, T, const CAPACITY: usize> serde::de::Visitor<'de> for Visitor<T, CAPACITY>
        where
            T: Copy + Default + serde::Deserialize<'de>,
        {
            type Value = StaticVec<T, CAPACITY>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "a sequence of at most {CAPACITY} entries")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = StaticVec::new();
                while let Some(value) = seq.next_element()? {
                    if !values.push(value) {
                        return Err(serde::de::Error::custom(format!(
                            "sequence exceeds StaticVec capacity {CAPACITY}"
                        )));
                    }
                }
                Ok(values)
            }
        }
        deserializer.deserialize_seq(Visitor::<T, CAPACITY>(core::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

    use super::*;
    use googletest::prelude::*;

    #[gtest]
    fn push_clear_and_deref() {
        let mut values = StaticVec::<u32, 3>::new();
        expect_true!(values.is_empty());
        expect_true!(values.push(1));
        expect_true!(values.push(2));
        assert_eq!(values.len(), 2);

        assert_eq!(&*values, &[1, 2]);

        values.clear();
        assert_true!(values.is_empty());
        assert_eq!(values.len(), 0);
    }

    #[gtest]
    fn full_push_returns_false() {
        let mut values = StaticVec::<u32, 2>::new();
        assert_true!(values.push(1));
        assert_true!(values.push(2));
        assert_true!(!values.push(3));
        assert_eq!(&*values, &[1, 2]);
    }

    #[gtest]
    fn test_pop() {
        let mut values = StaticVec::<u32, 2>::new();
        assert_true!(values.push(1));
        assert_true!(values.push(2));

        expect_that!(values.pop(), eq(Some(2)));
        expect_that!(values.pop(), eq(Some(1)));
        expect_that!(values.pop(), eq(None));
    }

    #[gtest]
    fn test_drop() {
        struct BumpsCountOnDrop<'a> {
            counter: &'a AtomicUsize,
        }

        impl<'a> Drop for BumpsCountOnDrop<'a> {
            fn drop(&mut self) {
                self.counter.fetch_add(1, Relaxed);
            }
        }

        let drop_counter = AtomicUsize::new(0);

        let mut values = StaticVec::<BumpsCountOnDrop, 3>::new();

        // Fill up to capacity
        for _ in 0..values.capacity() {
            assert_true!(values.push(BumpsCountOnDrop {
                counter: &drop_counter
            }));
        }
        expect_eq!(drop_counter.load(Relaxed), 0);

        assert_false!(values.push(BumpsCountOnDrop {
            counter: &drop_counter
        }));
        expect_eq!(
            drop_counter.load(Relaxed),
            1,
            "A failed push of a temporary would result in it being dropped"
        );

        values.pop();
        expect_eq!(drop_counter.load(Relaxed), 2);

        assert_that!(
            values.len,
            eq(2),
            "We expect two elements to be left in the vec"
        );
        values.clear();
        expect_eq!(
            drop_counter.load(Relaxed),
            4,
            "Clearing two elements should bump us to 4"
        );
    }

    #[gtest]
    fn const_support() {
        {
            const EMPTY_VEC: StaticVec<u32, 2> = StaticVec::new();

            let empty = EMPTY_VEC.is_empty();
            expect_true!(empty);
        }

        {
            const FULL_VEC: StaticVec<u32, 2> = StaticVec::new().push_const(1).push_const(2);
            let full = FULL_VEC.is_full();
            expect_true!(full);
            let len = FULL_VEC.len();
            expect_eq!(len, 2);
        }
    }

    #[gtest]
    fn fmt_debug() {
        let vec = StaticVec::<usize, 2>::new().push_const(1).push_const(2);

        expect_eq!(
            "StaticVec { CAPACITY: 2, len: 2, entries: [1, 2] }",
            format!("{:?}", vec)
        );
    }

    #[gtest]
    fn fmt_display() {
        let vec = StaticVec::<usize, 2>::new().push_const(1).push_const(2);

        expect_eq!("[1, 2]", format!("{}", vec));
    }

    #[cfg(feature = "serde")]
    #[gtest]
    fn serde_round_trip() {
        let mut values = StaticVec::<u32, 3>::new();
        values.push(1);
        values.push(2);
        let json = serde_json::to_string(&values).unwrap();
        assert_eq!(json, "[1,2]");
        let round_trip: StaticVec<u32, 3> = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, values);
    }

    #[cfg(feature = "serde")]
    #[gtest]
    fn serde_rejects_over_capacity() {
        let error = serde_json::from_str::<StaticVec<u32, 2>>("[1,2,3]").unwrap_err();
        assert_true!(error.to_string().contains("capacity 2"));
    }

    #[cfg(feature = "serde")]
    #[gtest]
    fn empty_serializes_as_array() {
        let values = StaticVec::<u32, 2>::new();
        assert_eq!(serde_json::to_string(&values).unwrap(), "[]");
    }
}
