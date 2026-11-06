use std::{
    any::type_name,
    borrow::Borrow,
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
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

impl<Tag> AsRef<str> for TaggedName<Tag> {
    fn as_ref(&self) -> &str {
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
}
