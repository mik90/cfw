use std::{
    any::type_name,
    borrow::Borrow,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    ops::Deref,
};

// We manually implement Clone, Debug, PartialEq, Eq, and Hash instead of
// deriving them: derives add `Tag: Trait` bounds for the Tag parameter
// without checking that the fields actually need them (the "imperfect
// derives" problem, https://github.com/rust-lang/rust/issues/26925), which
// would force every tag to implement all five traits or make any container
// of a name type fail to derive them. Manual impls are unconditional.
pub struct TaggedName<Tag> {
    value: String,
    // (PhantomData<fn() -> Tag> keeps auto traits like Send/Sync
    // unconditional, they would otherwise be inherited from Tag.)
    _marker: PhantomData<Tag>,
}

impl<Tag> TaggedName<Tag> {
    pub fn new(str: impl Into<String>) -> Self {
        Self {
            value: str.into(),
            _marker: PhantomData,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<Tag> Clone for TaggedName<Tag> {
    fn clone(&self) -> Self {
        Self::new(self.value.clone())
    }
}

impl<Tag> fmt::Debug for TaggedName<Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The tag type is the only name available at runtime (type aliases are
        // erased), so label the value with the tag's short name. Diagnostic
        // output only; type_name paths are not guaranteed to be stable.
        let tag = type_name::<Tag>()
            .rsplit("::")
            .next()
            .unwrap_or("TaggedName");
        f.debug_tuple(tag).field(&self.value).finish()
    }
}

impl<Tag> fmt::Display for TaggedName<Tag> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.value)
    }
}

impl<Tag> PartialEq for TaggedName<Tag> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<Tag> Eq for TaggedName<Tag> {}

impl<Tag> Hash for TaggedName<Tag> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state)
    }
}

impl<Tag> From<&str> for TaggedName<Tag> {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl<Tag> From<&String> for TaggedName<Tag> {
    fn from(value: &String) -> Self {
        Self::new(value.clone())
    }
}

impl<Tag> From<String> for TaggedName<Tag> {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl<Tag> PartialOrd for TaggedName<Tag> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<Tag> Ord for TaggedName<Tag> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}

impl<Tag> AsRef<str> for TaggedName<Tag> {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

// Lets &TaggedName coerce to &str at fn and method boundaries.
impl<Tag> Deref for TaggedName<Tag> {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

// Allows for lookup as key in a hashmap using a str
impl<Tag> Borrow<str> for TaggedName<Tag> {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl<Tag> PartialEq<&str> for TaggedName<Tag> {
    fn eq(&self, other: &&str) -> bool {
        self.as_str().eq(*other)
    }
}

impl<Tag> PartialEq<str> for TaggedName<Tag> {
    fn eq(&self, other: &str) -> bool {
        self.as_str().eq(other)
    }
}

// Serde impls round-trip the plain string form, matching the recorded log
// format. Manual impls again: derives would add `Tag:` bounds (see above),
// and serde_json would serialize the PhantomData field's presence in the
// struct layout itself.
#[cfg(feature = "serde")]
impl<Tag> serde::Serialize for TaggedName<Tag> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de, Tag> serde::Deserialize<'de> for TaggedName<Tag> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct NameVisitor<Tag>(PhantomData<Tag>);
        impl<'de, Tag> serde::de::Visitor<'de> for NameVisitor<Tag> {
            type Value = TaggedName<Tag>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a string name")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(TaggedName::new(value))
            }
        }
        deserializer.deserialize_string(NameVisitor(PhantomData))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    struct DummyTag;
    type DummyName = TaggedName<DummyTag>;

    #[gtest]
    fn from_str() {
        let name = "dummy_name";
        expect_eq!(DummyName::new(name).as_str(), "dummy_name");
    }

    #[gtest]
    fn debug_shows_tag_name() {
        let formatted = format!("{:?}", DummyName::new("dummy_name"));
        expect_that!(formatted, eq("DummyTag(\"dummy_name\")"));
    }

    #[gtest]
    fn display_shows_name() {
        let formatted = format!("{}", DummyName::new("dummy_name"));
        expect_that!(formatted, eq("dummy_name"));
    }

    #[gtest]
    fn from_string() {
        expect_eq!(DummyName::from("dummy".to_string()).as_str(), "dummy");
    }

    #[gtest]
    fn orders_lexically() {
        let mut names = [DummyName::new("b"), DummyName::new("a")];
        names.sort();
        expect_eq!(
            names.iter().map(AsRef::as_ref).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[gtest]
    fn serde_round_trip() {
        #[derive(serde::Serialize, serde::Deserialize, Debug)]
        struct Container {
            name: DummyName,
            names: Vec<DummyName>,
        }
        let value = Container {
            name: DummyName::new("single"),
            names: vec![DummyName::new("a"), DummyName::new("b")],
        };
        let json = serde_json::to_string(&value).unwrap();
        expect_that!(json, eq(r#"{"name":"single","names":["a","b"]}"#));
        let decoded: Container = serde_json::from_str(&json).unwrap();
        expect_eq!(decoded.name, value.name);
        expect_eq!(decoded.names, value.names);
    }

    #[gtest]
    fn serde_rejects_non_strings() {
        #[derive(serde::Deserialize)]
        struct Container {
            name: DummyName,
        }
        let message = serde_json::from_str::<Container>(r#"{"name": 42}"#)
            .err()
            .unwrap()
            .to_string();
        expect_that!(message, contains_substring("a string name"));
    }
}
