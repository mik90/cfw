use std::collections::HashMap;
use std::mem;
use std::sync::{Arc, RwLock};

/// Allows for interning strings such as channel names and callback names
/// Copied from https://matklad.github.io/2020/03/22/fast-simple-rust-interner.html
pub struct StringInterner {
    /// 'static is used to fake interior references
    string_to_hash: HashMap<&'static str, InternId>,
    /// References to interned strings
    interned_strings: Vec<&'static str>,
    /// Other containers point to this buffer
    contiguous_storage: String,
    /// Old storage containeres that weren't big enough
    overflow_storage: Vec<String>,
}

impl StringInterner {
    pub fn with_capacity(char_capacity: usize) -> StringInterner {
        let cap = char_capacity.next_power_of_two();
        StringInterner {
            string_to_hash: HashMap::default(),
            interned_strings: Vec::new(),
            contiguous_storage: String::with_capacity(cap),
            overflow_storage: Vec::new(),
        }
    }

    pub fn intern(&mut self, value: &str) -> InternId {
        if let Some(&intern_id) = self.string_to_hash.get(value) {
            return intern_id;
        }
        // SAFETY: Strings are kept alive by our invariants when used internally
        let name = unsafe { self.alloc(value) };
        // IDs are just the entry in the map. u32 max strings would be a _huge_ amount,
        // so u32 is fine.
        let id = InternId::new(self.string_to_hash.len() as u32);
        self.string_to_hash.insert(name, id);
        self.interned_strings.push(name);

        debug_assert!(self.lookup(id) == name);
        debug_assert!(self.intern(name) == id);

        id
    }

    pub fn lookup(&self, intern_id: InternId) -> &str {
        self.interned_strings[intern_id.id as usize]
    }

    unsafe fn alloc(&mut self, name: &str) -> &'static str {
        let cap = self.contiguous_storage.capacity();
        if cap < self.contiguous_storage.len() + name.len() {
            // Not enough space for this new string
            let new_cap = (cap.max(name.len()) + 1).next_power_of_two();
            let new_buf = String::with_capacity(new_cap);
            // Replace our current storage with the new buf
            let old_buf = mem::replace(&mut self.contiguous_storage, new_buf);
            // Add our old storage to the overflow
            self.overflow_storage.push(old_buf);
        }

        let interned = {
            let start = self.contiguous_storage.len();
            self.contiguous_storage.push_str(name);
            &self.contiguous_storage[start..]
        };

        // SAFETY: We ensure that all interned strings stay alive in our main/overflow buffers
        unsafe { &*(interned as *const str) }
    }
}

/// Strong-type for accessing interned strings
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct InternId {
    id: u32,
}

impl InternId {
    fn new(id: u32) -> InternId {
        InternId { id }
    }
}

pub struct SharedStringInterner {
    interner: Arc<RwLock<StringInterner>>,
}

impl SharedStringInterner {
    pub fn with_capacity(char_capacity: usize) -> SharedStringInterner {
        SharedStringInterner {
            interner: Arc::new(RwLock::new(StringInterner::with_capacity(char_capacity))),
        }
    }

    pub fn intern(&self, value: &str) -> InternId {
        self.interner
            .write()
            .expect("Interner RwLock is poisioned")
            .intern(value)
    }

    pub fn lookup(&self, intern_id: InternId, f: impl FnOnce(&str)) {
        let interner_guard = self.interner.read().expect("Interner RwLock is poisioned");
        let value = interner_guard.lookup(intern_id);
        f(value);
    }
}

#[cfg(test)]
mod tests {
    use crate::string_interner::StringInterner;

    #[test]
    fn test_intern() {
        let mut interner = StringInterner::with_capacity(10);
        let interned_hello = interner.intern("hello");

        let lookup_result = interner.lookup(interned_hello);

        assert_eq!(lookup_result, "hello");
    }

    #[test]
    fn test_over_capacity() {
        let hello_string = "hello";
        let mut interner = StringInterner::with_capacity(hello_string.len());
        let interned_hello = interner.intern(hello_string);
        assert_eq!(interner.overflow_storage.len(), 0);
        let interned_world = interner.intern("world");
        assert_eq!(interner.overflow_storage.len(), 1);

        let lookup_hello = interner.lookup(interned_hello);
        assert_eq!(lookup_hello, "hello");
        let lookup_world = interner.lookup(interned_world);
        assert_eq!(lookup_world, "world");
    }

    #[test]
    fn test_many_interns() {
        let mut interner = StringInterner::with_capacity(100);
        let mut intern_ids = vec![];
        for i in 0..100 {
            intern_ids.push(interner.intern(&i.to_string()));
        }

        for (i, intern_id) in intern_ids.iter().enumerate() {
            assert_eq!(interner.lookup(*intern_id), i.to_string());
        }
    }
}
