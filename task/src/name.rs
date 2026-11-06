#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TaggedName<Tag> {
    value: String,
    _marker: std::marker::PhantomData<fn() -> Tag>,
}

use std::{borrow::Borrow, marker::PhantomData};

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

impl<Tag> From<&str> for TaggedName<Tag> {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl<Tag> AsRef<str> for TaggedName<Tag> {
    fn as_ref(&self) -> &str {
        &self.value
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

// Allows for lookup of ChannelName as key in hashmap using a str
impl<Tag> Borrow<str> for TaggedName<Tag> {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use googletest::prelude::*;

    struct DummyTag {}
    type DummyName = TaggedName<DummyTag>;

    #[gtest]
    fn from_st() {
        let name = "callback";
        expect_eq!(DummyName::new(name).as_str(), "channel");
    }
}
