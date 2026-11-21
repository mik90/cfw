use crate::StaticVec;

pub struct StaticString<const CAPACITY: usize> {
    static_vec: StaticVec<u8, CAPACITY>,
}

pub struct TruncatedStrResult<const CAPACITY: usize> {
    pub string: StaticString<CAPACITY>,
    pub dropped_bytes: usize,
}

impl<const CAPACITY: usize> StaticString<CAPACITY> {
    pub const fn new() -> Self {
        Self {
            static_vec: StaticVec::new(),
        }
    }

    pub const fn is_empty(&self) -> bool {
        self.static_vec.is_empty()
    }

    pub const fn len(&self) -> usize {
        self.static_vec.len()
    }

    /// Returns string and number of dropped bytes
    pub fn from_str_truncated(s: &str) -> (Self, usize) {
        let mut string = Self::new();

        // Last index in order to keep the str as utf8
        let str_end_index = s.floor_char_boundary(CAPACITY);

        // the str full length is at least as long as the index of the last valid char.
        // We already got the last index based on the capacity bounds, so the last index
        // won't be greater than capacity.
        let dropped_bytes = s.len() - str_end_index;

        // If we can't parse the truncated str as utf8, just return the error now
        let truncated_str = &s[..str_end_index];

        // Using floor_char_boundary ensures that we get valid utf8 from the input string
        for b in truncated_str.as_bytes() {
            string.static_vec.push(*b);
        }
        (string, dropped_bytes)
    }

    pub fn as_str(&self) -> &str {
        // SAFETY: We made sure that the input str was utf8 in [`from_str_truncated`]
        unsafe { str::from_utf8_unchecked(self.static_vec.as_slice()) }
    }
}

impl<const CAPACITY: usize> core::fmt::Debug for StaticString<CAPACITY> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StaticString")
            .field("CAPACITY", &CAPACITY)
            .field("len", &self.len())
            .field("as_str", &self.as_str())
            .finish()
    }
}

impl<const CAPACITY: usize> core::fmt::Display for StaticString<CAPACITY> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl<const CAPACITY: usize> Default for StaticString<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CAPACITY: usize> PartialEq for StaticString<CAPACITY> {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl<const CAPACITY: usize> PartialEq<&str> for StaticString<CAPACITY> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl<const CAPACITY: usize> PartialEq<alloc::string::String> for StaticString<CAPACITY> {
    fn eq(&self, other: &alloc::string::String) -> bool {
        self.as_str() == other
    }
}

impl<const CAPACITY: usize> TryFrom<&str> for StaticString<CAPACITY> {
    type Error = usize;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let (truncated, dropped) = Self::from_str_truncated(value);
        if dropped > 0 {
            Err(dropped)
        } else {
            Ok(truncated)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    #[gtest]
    fn construct_empty() {
        let string = StaticString::<5>::new();
        expect_true!(string.is_empty());
        expect_eq!(string.len(), 0);
    }

    #[gtest]
    fn equal_size_to_capacity() {
        let (string, dropped) = StaticString::<5>::from_str_truncated("hello");
        expect_eq!(dropped, 0);
        expect_eq!(string, "hello");
    }

    #[gtest]
    fn greater_size_than_capacity() {
        let (string, dropped) = StaticString::<2>::from_str_truncated("hello");
        expect_eq!(dropped, 3);
        expect_eq!(string, "he");
    }

    #[gtest]
    fn less_size_than_capacity() {
        let (string, dropped) = StaticString::<10>::from_str_truncated("hello");
        expect_eq!(dropped, 0);
        expect_eq!(string, "hello");
    }

    #[gtest]
    fn truncate_at_boundary() {
        let full_string = "❤️🧡💛💚💙💜";

        expect_eq!(full_string.len(), 26);
        assert_false!(
            full_string.is_char_boundary(13),
            "find a byte index that cuts off an emoji"
        );

        let expected_string = "❤️🧡";
        assert_that!(expected_string.len(), eq(10));
        let (truncated_string, dropped_bytes) = StaticString::<10>::from_str_truncated(full_string);
        expect_eq!(
            dropped_bytes,
            16,
            "Drop off the byte size difference between the full and expected string"
        );
        expect_eq!(truncated_string, expected_string);
    }

    #[gtest]
    fn fmt_display() {
        let (string, drops) = StaticString::<12>::from_str_truncated("some data");
        expect_eq!(drops, 0);
        expect_eq!("some data", format!("{}", string));
    }

    #[gtest]
    fn fmt_debug() {
        let (string, drops) = StaticString::<12>::from_str_truncated("some data");
        expect_eq!(drops, 0);
        expect_eq!(
            "StaticString { CAPACITY: 12, len: 9, as_str: \"some data\" }",
            format!("{:?}", string)
        );
    }
}
