//! Fixed-capacity inline sequence.
use std::ops::Deref;

/// Inline sequence of at most `N` entries. Never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedVec<T: Copy + Default, const N: usize> {
    entries: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> BoundedVec<T, N> {
    pub fn new() -> Self {
        Self {
            entries: [T::default(); N],
            len: 0,
        }
    }

    /// Append `value`, returning false when the sequence is full.
    pub fn push(&mut self, value: T) -> bool {
        if self.len == N {
            return false;
        }
        self.entries[self.len] = value;
        self.len += 1;
        true
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn as_slice(&self) -> &[T] {
        &self.entries[..self.len]
    }
}

impl<T: Copy + Default, const N: usize> Default for BoundedVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default, const N: usize> Deref for BoundedVec<T, N> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

#[cfg(feature = "serde")]
impl<T: Copy + Default + serde::Serialize, const N: usize> serde::Serialize for BoundedVec<T, N> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.as_slice(), serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, T, const N: usize> serde::Deserialize<'de> for BoundedVec<T, N>
where
    T: Copy + Default + serde::Deserialize<'de>,
{
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T, const N: usize>(std::marker::PhantomData<T>);
        impl<'de, T, const N: usize> serde::de::Visitor<'de> for Visitor<T, N>
        where
            T: Copy + Default + serde::Deserialize<'de>,
        {
            type Value = BoundedVec<T, N>;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "a sequence of at most {N} entries")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = BoundedVec::new();
                while let Some(value) = seq.next_element()? {
                    if !values.push(value) {
                        return Err(serde::de::Error::custom(format!(
                            "sequence exceeds BoundedVec capacity {N}"
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
        let mut values = BoundedVec::<u32, 3>::new();
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
        let mut values = BoundedVec::<u32, 2>::new();
        assert!(values.push(1));
        assert!(values.push(2));
        assert!(!values.push(3));
        assert_eq!(&*values, &[1, 2]);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_round_trip() {
        let mut values = BoundedVec::<u32, 3>::new();
        values.push(1);
        values.push(2);
        let json = serde_json::to_string(&values).unwrap();
        assert_eq!(json, "[1,2]");
        let round_trip: BoundedVec<u32, 3> = serde_json::from_str(&json).unwrap();
        assert_eq!(round_trip, values);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn serde_rejects_over_capacity() {
        let error = serde_json::from_str::<BoundedVec<u32, 2>>("[1,2,3]").unwrap_err();
        assert!(error.to_string().contains("capacity 2"));
    }

    #[cfg(feature = "serde")]
    #[test]
    fn empty_serializes_as_array() {
        let values = BoundedVec::<u32, 2>::new();
        assert_eq!(serde_json::to_string(&values).unwrap(), "[]");
    }
}
