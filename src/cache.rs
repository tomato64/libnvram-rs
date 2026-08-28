//! Tier 2: this process's private materialised view.
//!
//! Exists for one reason: pointer stability. `nvram_get` must hand back a
//! pointer the caller can hold without ever freeing it, and a pointer into
//! the shared arena would rot the moment any *other* process triggered a
//! compaction. Copying into private memory removes that whole class of bug.
//!
//! Populated lazily - only keys this process actually reads.

use std::collections::HashMap;

/// One cached key.
///
/// `value` is a heap allocation whose address is what callers hold, so it
/// must not be reallocated in place. Replacing it moves the old buffer to
/// `retired`, which is released one refresh later - that grace period is what
/// makes a pointer survive a change to its key.
pub struct Entry {
    pub value: Option<Box<[u8]>>,
    pub retired: Option<Box<[u8]>>,
    pub slot_seen: u64,
    /// False until the entry has been resolved against the segment at least
    /// once, so a freshly interned entry is never mistaken for a valid miss.
    pub valid: bool,
}

impl Entry {
    fn new() -> Entry {
        Entry {
            value: None,
            retired: None,
            slot_seen: 0,
            valid: false,
        }
    }

    /// Pointer handed to C. NUL-terminated; null when the key is absent.
    pub fn as_ptr(&self) -> *const libc::c_char {
        match &self.value {
            Some(v) => v.as_ptr() as *const libc::c_char,
            None => std::ptr::null(),
        }
    }

    /// Install a freshly read value, keeping the pointer stable when the
    /// bytes are unchanged.
    pub fn install(&mut self, fresh: Option<Vec<u8>>, slot_seen: u64) {
        let unchanged = match (&self.value, &fresh) {
            // Compare against the stored value minus its NUL terminator.
            (Some(cur), Some(new)) => cur.len() == new.len() + 1 && &cur[..new.len()] == &new[..],
            (None, None) => true,
            _ => false,
        };

        if !unchanged {
            self.retired = self.value.take();
            self.value = fresh.map(|mut v| {
                v.push(0);
                v.into_boxed_slice()
            });
        }

        self.slot_seen = slot_seen;
        self.valid = true;
    }
}

#[derive(Default)]
pub struct Cache {
    map: HashMap<Vec<u8>, Box<Entry>>,
}

impl Cache {
    pub fn new() -> Cache {
        Cache {
            map: HashMap::new(),
        }
    }

    pub fn entry(&mut self, key: &[u8]) -> &mut Entry {
        if !self.map.contains_key(key) {
            self.map.insert(key.to_vec(), Box::new(Entry::new()));
        }
        self.map.get_mut(key).expect("just inserted")
    }

    /// Drop every cached value. Pointers already handed out stay valid for
    /// one more cycle via `retired`; anything older is the caller's problem,
    /// which matches the documented Broadcom contract.
    pub fn invalidate_all(&mut self) {
        for e in self.map.values_mut() {
            e.retired = e.value.take();
            e.valid = false;
            e.slot_seen = 0;
        }
    }
}
