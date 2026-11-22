//! Fixed-capacity inline UTF8 string.
use core::debug_assert_matches;

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

    /// Builds a string from a compile-time `&str`, asserting that it fits.
    ///
    /// This will be very inefficient outside of a const context.
    ///
    /// Prefer the [`static_string!`](crate::static_string!) macro, which sizes the
    /// capacity from the input.
    pub const fn from_str_const(s: &str) -> Self {
        assert!(s.len() <= CAPACITY, "string exceeds StaticString capacity");

        let bytes = s.as_bytes();
        let mut static_vec = StaticVec::new();
        let mut i = 0;
        while i < bytes.len() {
            static_vec = static_vec.push_const(bytes[i]);
            i += 1;
        }

        Self { static_vec }
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
        // We only allow for creating utf8 representations, but we can double check
        // for utf8 validity in debug.
        debug_assert_matches!(str::from_utf8(&self.static_vec), Ok(_));

        // SAFETY: Every constructor only appends bytes taken from valid `str` values.
        unsafe { str::from_utf8_unchecked(self.static_vec.as_slice()) }
    }

    /// Appends `s`, returning false and leaving the string unchanged when it does not fit.
    ///
    /// Concatenating two valid UTF8 strings is valid UTF8, so this needs no checks on
    /// the appended bytes.
    pub fn push_str(&mut self, s: &str) -> bool {
        if s.len() > (CAPACITY - self.len()) {
            return false;
        }

        self.push_str_truncated(s);
        true
    }

    /// Appends the prefix of `s` that fits, truncating at a UTF8 boundary.
    ///
    /// Returns the number of bytes that did not fit.
    pub fn push_str_truncated(&mut self, s: &str) -> usize {
        let str_end_index = s.floor_char_boundary(CAPACITY - self.len());
        let dropped_bytes = s.len() - str_end_index;

        for byte in &s.as_bytes()[..str_end_index] {
            self.static_vec.push(*byte);
        }

        dropped_bytes
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

#[cfg(feature = "serde")]
impl<const CAPACITY: usize> serde::Serialize for StaticString<CAPACITY> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.as_str(), serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, const CAPACITY: usize> serde::Deserialize<'de> for StaticString<CAPACITY> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<const CAPACITY: usize>();

        impl<'de, const CAPACITY: usize> serde::de::Visitor<'de> for Visitor<CAPACITY> {
            type Value = StaticString<CAPACITY>;

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "a static string of at most {CAPACITY} bytes")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let (string, dropped_bytes) = StaticString::<CAPACITY>::from_str_truncated(v);
                if dropped_bytes > CAPACITY {
                    return Err(serde::de::Error::custom(format!(
                        "sequence exceeds StaticVec capacity {CAPACITY}"
                    )));
                }

                Ok(string)
            }
        }
        deserializer.deserialize_str(Visitor::<CAPACITY> {})
    }
}

/// Creates a [`StaticString`] with capacity equal to the byte length of the input.
///
/// The input must be a constant expression (a literal, `concat!`, a `const`
/// item, and so on) so its length can size the string. Works in const contexts.
///
/// To create a static string usable in either runtime or const contexts:
///
/// ```
/// const HELLO: base::StaticString<5> = base::static_string!("hello");
/// assert_eq!(HELLO.as_str(), "hello");
/// ```
///
/// To create a new const variable, so the capacity does not have to be written out (`pub` is
/// supported):
///
/// ```
/// base::static_string!(const HELLO = "hello");
/// assert_eq!(HELLO.as_str(), "hello");
/// ```
#[macro_export]
macro_rules! static_string {
    ($vis:vis const $name:ident = $s:expr $(,)?) => {
        $vis const $name: $crate::StaticString<{ ($s).len() }> =
            $crate::StaticString::from_str_const($s);
    };
    ($s:expr $(,)?) => {{
        const S: &str = $s;
        const { $crate::StaticString::<{ S.len() }>::from_str_const(S) }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    static_string!(pub const PUBLIC_HELLO = "hello");

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
    fn push_str_appends() {
        let mut string = StaticString::<11>::from_str_truncated("hello").0;
        expect_true!(string.push_str(" world"));
        expect_eq!(string, "hello world");
        expect_eq!(string.len(), 11);
    }

    #[gtest]
    fn push_str_rejects_when_it_does_not_fit() {
        let mut string = StaticString::<5>::from_str_truncated("hello").0;
        expect_false!(string.push_str("!"));
        expect_eq!(string, "hello");
    }

    #[gtest]
    fn push_str_truncated_appends_whole_string_when_it_fits() {
        let mut string = StaticString::<11>::from_str_truncated("hello").0;
        expect_eq!(string.push_str_truncated(" world"), 0);
        expect_eq!(string, "hello world");
    }

    #[gtest]
    fn push_str_truncated_drops_overflow() {
        let mut string = StaticString::<8>::from_str_truncated("hello").0;
        expect_eq!(string.push_str_truncated(" world"), 3);
        expect_eq!(string, "hello wo");
        expect_eq!(string.len(), 8);
    }

    #[gtest]
    fn push_str_truncated_respects_utf8_boundaries() {
        let mut string = StaticString::<2>::new();
        expect_eq!(string.push_str_truncated("aé"), 2);
        expect_eq!(string, "a");
    }

    #[gtest]
    fn push_str_truncated_drops_everything_when_full() {
        let mut string = StaticString::<5>::from_str_truncated("hello").0;
        expect_eq!(string.push_str_truncated("world"), 5);
        expect_eq!(string, "hello");
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

    #[gtest]
    fn macro_constructs_string() {
        let string = static_string!("hello");
        expect_eq!(string.len(), 5);
        expect_eq!(string.as_str(), "hello");
    }

    #[gtest]
    fn macro_works_in_const_context() {
        const HELLO: StaticString<5> = static_string!("hello");
        expect_eq!(HELLO.as_str(), "hello");
        expect_eq!(HELLO.len(), 5);
    }

    #[gtest]
    fn macro_const_item_infers_capacity() {
        static_string!(const HELLO = "hello");
        expect_eq!(HELLO.len(), 5);
        expect_eq!(HELLO.as_str(), "hello");

        static_string!(const ACCENTED = "héllo",);
        expect_eq!(ACCENTED.len(), 6);
        expect_eq!(ACCENTED.as_str(), "héllo");
    }

    #[gtest]
    fn macro_pub_const_item() {
        expect_eq!(PUBLIC_HELLO.len(), 5);
        expect_eq!(PUBLIC_HELLO.as_str(), "hello");
    }

    #[gtest]
    fn macro_uses_byte_length() {
        let string = static_string!("héllo",);
        expect_eq!(string.len(), 6);
        expect_eq!(string.as_str(), "héllo");
    }

    #[gtest]
    fn macro_supports_empty() {
        let string = static_string!("");
        expect_true!(string.is_empty());
        expect_eq!(string.as_str(), "");
    }

    #[gtest]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "Utf8Error")]
    fn assert_on_invalid_utf8() {
        // Invalid utf8 representation from the str docs
        let invalid_sparkle_heart_bytes = vec![0, 159, 146, 150];

        let invalid_sparkle_heart_str =
            // SAFETY: This is actually not safe, but we're testing the case where someone has been unsafe in debug
            unsafe { str::from_utf8_unchecked(&invalid_sparkle_heart_bytes) };

        let (string, dropped) = StaticString::<10>::from_str_truncated(invalid_sparkle_heart_str);
        expect_eq!(dropped, 0);

        string.as_str();
    }

    #[cfg(feature = "serde")]
    #[gtest]
    fn serde_round_trip() {
        let (input, dropped) = StaticString::<20>::from_str_truncated("hi");
        assert_that!(dropped, eq(0));

        let json = serde_json::to_string(&input).unwrap();
        expect_eq!(json, "\"hi\"");

        let round_trip: StaticString<20> = serde_json::from_str(&json).unwrap();
        expect_eq!(round_trip, input);
    }

    #[cfg(feature = "serde")]
    #[gtest]
    fn serde_rejects_over_capacity() {
        let (input, dropped) = StaticString::<5>::from_str_truncated("hello");
        assert_that!(dropped, eq(0));

        let json = serde_json::to_string(&input).unwrap();
        expect_eq!(json, "\"hello\"");

        let round_trip = serde_json::from_str::<StaticString<2>>(&json);
        assert_true!(round_trip.is_err());
        let err_string = format!("{}", round_trip.unwrap_err());
        expect_that!(
            err_string,
            contains_substring("sequence exceeds StaticVec capacity 2")
        );
    }
}
