use std::cmp::{Eq, PartialEq};
use std::collections::HashMap;
use std::hash::Hash;
use std::marker::PhantomData;

/// Marker type distinguishing interned callback names from interned channel
/// names, so [`StringInterner`] lookups stay strongly typed.
#[derive(Clone, Debug)]
pub struct CallbackNameTag {}

/// Marker type distinguishing interned channel names from interned callback
/// names, so [`StringInterner`] lookups stay strongly typed.
#[derive(Clone, Debug)]
pub struct ChannelNameTag {}

pub type CallbackNameInterner = StringInterner<CallbackNameTag>;
pub type ChannelNameInterner = StringInterner<ChannelNameTag>;

/// Strong-type for accessing interned channel names or task names
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InternId<MarkerType> {
    id: u32,
    _marker: PhantomData<MarkerType>,
}

impl<MarkerType> Clone for InternId<MarkerType> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<MarkerType> Copy for InternId<MarkerType> {}

impl<MarkerType> InternId<MarkerType> {
    fn new(id: u32) -> InternId<MarkerType> {
        InternId {
            id,
            _marker: Default::default(),
        }
    }
}

impl<MarkerType> PartialEq for InternId<MarkerType> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<MarkerType> Eq for InternId<MarkerType> {}

impl<MarkerType> Hash for InternId<MarkerType> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Bidirectional map for looking up data given ID, or ID given data
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct StringInterner<InternType> {
    id_to_data: HashMap<InternId<InternType>, String>,
    data_to_id: HashMap<String, InternId<InternType>>,
}

impl<InternType> StringInterner<InternType> {
    pub fn new() -> Self {
        StringInterner {
            id_to_data: HashMap::default(),
            data_to_id: HashMap::default(),
        }
    }

    pub fn intern(&mut self, value: &str) -> InternId<InternType> {
        if let Some(&intern_id) = self.data_to_id.get(value) {
            return intern_id;
        }
        // IDs are just the entry in the map. u32 max strings would be a _huge_ amount,
        // so u32 is fine.
        let id = InternId::new(self.data_to_id.len() as u32);

        self.id_to_data.insert(id, value.into());
        self.data_to_id.insert(value.into(), id);

        id
    }

    pub fn lookup_by_id(&self, intern_id: InternId<InternType>) -> &str {
        self.id_to_data
            .get(&intern_id)
            .expect("InternIds should guarantee that values are always present")
    }

    /// May not return an ID since we're using possibly un-interned strings
    pub fn lookup_by_value(&self, value: &str) -> Option<InternId<InternType>> {
        self.data_to_id.get(value).copied()
    }

    /// Minimizes size before passing to users, should not increase in size since users willlll
    /// get this as immutable
    pub fn shrink_to_fit(&mut self) {
        self.id_to_data.shrink_to_fit();
        self.data_to_id.shrink_to_fit();
    }
}

impl<InternType> Default for StringInterner<InternType> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::string_interner::StringInterner;

    #[derive(Debug)]
    struct DummyMarker {}
    type TestInterner = StringInterner<DummyMarker>;

    #[test]
    fn test_intern() {
        let mut interner = TestInterner::new();
        let interned_hello = interner.intern("hello");
        let interned_world = interner.intern("world");
        interner.shrink_to_fit();

        assert_eq!(interner.lookup_by_id(interned_hello), "hello");
        assert_eq!(interner.lookup_by_id(interned_world), "world");

        assert_eq!(interner.lookup_by_value("hello").unwrap(), interned_hello);
        assert_eq!(interner.lookup_by_value("world").unwrap(), interned_world);
        assert!(interner.lookup_by_value("not-there").is_none());
    }
}
