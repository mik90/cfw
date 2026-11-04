//! Fixed-capacity inline sequence.
use std::mem;
use std::ops::Deref;
use std::slice;

/// Inline sequence of at most `N` entries. Never allocates.
#[derive(Debug)]
pub struct StaticVec<T, const N: usize> {
    /// Array of possibly initialized entries
    entries: [mem::MaybeUninit<T>; N],
    len: usize,
}

impl<T: Clone, const N: usize> Clone for StaticVec<T, N> {
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

impl<T: PartialEq, const N: usize> PartialEq for StaticVec<T, N> {
    fn eq(&self, other: &Self) -> bool {
        let initialized_iter = self.as_slice().iter();

        let other_initialized_iter = other.as_slice().iter();

        initialized_iter.eq(other_initialized_iter)
    }
}

impl<T, const N: usize> StaticVec<T, N> {
    pub fn new() -> Self {
        Self {
            // Const block allows this to build for some reason, presumably because
            // it creates a new uninit instead of copying the same uninit repeatedly.
            entries: [const { mem::MaybeUninit::uninit() }; N],
            len: 0,
        }
    }

    /// Append `value`, returning false when the sequence is full.
    pub fn push(&mut self, value: T) -> bool {
        if self.len == N {
            return false;
        }
        self.entries[self.len] = mem::MaybeUninit::new(value);
        self.len += 1;
        true
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

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
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

impl<T, const N: usize> Drop for StaticVec<T, N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<T: Clone, const N: usize> StaticVec<T, N> {
    pub fn push_cloned(&mut self, value: &T) -> bool {
        if self.len == N {
            return false;
        }
        self.entries[self.len] = mem::MaybeUninit::new(value.clone());
        self.len += 1;
        true
    }
}

impl<T: Copy + Default, const N: usize> Default for StaticVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default, const N: usize> Deref for StaticVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

#[cfg(feature = "serde")]
impl<T: Copy + Default + serde::Serialize, const N: usize> serde::Serialize for StaticVec<T, N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.as_slice(), serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T, const N: usize> serde::Deserialize<'de> for StaticVec<T, N>
where
    T: Copy + Default + serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T, const N: usize> serde::de::Visitor<'de> for Visitor<T, N>
        where
            T: Copy + Default + serde::Deserialize<'de>,
        {
            type Value = StaticVec<T, N>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a sequence of at most {N} entries")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = StaticVec::new();
                while let Some(value) = seq.next_element()? {
                    if !values.push(value) {
                        return Err(serde::de::Error::custom(format!(
                            "sequence exceeds StaticVec capacity {N}"
                        )));
                    }
                }
                Ok(values)
            }
        }
        deserializer.deserialize_seq(Visitor::<T, N>(std::marker::PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_clear_and_deref() {
        let mut values = StaticVec::<u32, 3>::new();
        assert!(values.is_empty());
        assert!(values.push(1));
        assert!(values.push(2));
        assert_eq!(values.len(), 2);
        assert_eq!(&*values, &[1, 2]);
        values.clear();
        assert!(values.is_empty());
        assert_eq!(values.len(), 0);
    }

    #[test]
    fn full_push_returns_false() {
        let mut values = StaticVec::<u32, 2>::new();
        assert!(values.push(1));
        assert!(values.push(2));
        assert!(!values.push(3));
        assert_eq!(&*values, &[1, 2]);
    }

    #[cfg(feature = "serde")]
    #[test]
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
    #[test]
    fn serde_rejects_over_capacity() {
        let error = serde_json::from_str::<StaticVec<u32, 2>>("[1,2,3]").unwrap_err();
        assert!(error.to_string().contains("capacity 2"));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn empty_serializes_as_array() {
        let values = StaticVec::<u32, 2>::new();
        assert_eq!(serde_json::to_string(&values).unwrap(), "[]");
    }
}
