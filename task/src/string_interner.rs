use std::collections::HashMap;
use std::mem;

/// Allows for interning strings such as channel names and callback names
/// Copied from https://matklad.github.io/2020/03/22/fast-simple-rust-interner.html
pub struct StringInterner {
    /// 'static is used to fake interior references
    string_to_hash: HashMap<&'static str, u32>,

    vec: Vec<&'static str>,

    buf: String,
    full: Vec<String>,
}

impl StringInterner {
    pub fn with_capacity(cap: usize) -> StringInterner {
        let cap = cap.next_power_of_two();
        StringInterner {
            string_to_hash: HashMap::default(),
            vec: Vec::new(),
            buf: String::with_capacity(cap),
            full: Vec::new(),
        }
    }

    pub fn intern(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.string_to_hash.get(name) {
            return id;
        }
        // SAFETY: Strings are kept alive by our invariants when used internally
        let name = unsafe { self.alloc(name) };
        let id = self.string_to_hash.len() as u32;
        self.string_to_hash.insert(name, id);
        self.vec.push(name);

        debug_assert!(self.lookup(id) == name);
        debug_assert!(self.intern(name) == id);

        id
    }

    pub fn lookup(&self, id: u32) -> &str {
        self.vec[id as usize]
    }

    unsafe fn alloc(&mut self, name: &str) -> &'static str {
        let cap = self.buf.capacity();
        if cap < self.buf.len() + name.len() {
            let new_cap = (cap.max(name.len()) + 1).next_power_of_two();
            let new_buf = String::with_capacity(new_cap);
            let old_buf = mem::replace(&mut self.buf, new_buf);
            self.full.push(old_buf);
        }

        let interned = {
            let start = self.buf.len();
            self.buf.push_str(name);
            &self.buf[start..]
        };

        // SAFETY: We ensure that all interned strings stay alive
        unsafe { &*(interned as *const str) }
    }
}

#[cfg(test)]
mod tests {}
