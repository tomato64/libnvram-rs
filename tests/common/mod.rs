#![allow(dead_code)]

//! Scratch-store harness.
//!
//! Each test gets its own directory and its own shared-memory object so the
//! suite can run without touching /nvram and without tests colliding.

use std::ffi::{CStr, CString};
use std::path::PathBuf;

pub struct Scratch {
    pub root: PathBuf,
    pub shm: String,
}

impl Scratch {
    pub fn new(tag: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("nvram-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch dir");

        let shm = format!("/nvram-test-{}-{}", tag, std::process::id());
        nvram::__set_paths(root.to_str().unwrap(), &shm);
        nvram::__unlink_segment();
        nvram::__reset_process_store();

        let s = Scratch { root, shm };
        // Model rc: bring the shared store up explicitly, once, before
        // anything reads a setting. Nothing else creates the segment.
        s.init();
        s
    }

    /// Same, but without bringing the shared store up - models the window
    /// before `rc` reaches `nvram_init()`, where calls run detached and go
    /// straight to the files.
    pub fn new_uninitialised(tag: &str) -> Scratch {
        let root = std::env::temp_dir().join(format!("nvram-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch dir");

        let shm = format!("/nvram-test-{}-{}", tag, std::process::id());
        nvram::__set_paths(root.to_str().unwrap(), &shm);
        nvram::__unlink_segment();
        nvram::__reset_process_store();

        Scratch { root, shm }
    }

    /// `nvram_init()`. Returns true on success.
    pub fn init(&self) -> bool {
        nvram::nvram_init(std::ptr::null_mut()) == 1
    }

    /// Forget the mapped segment without destroying it - models a process
    /// restart against a store that is still live. The new process attaches;
    /// it does not create.
    pub fn restart_process(&self) {
        nvram::__reset_process_store();
    }

    /// Destroy the segment as well - models a reboot, where tmpfs is lost and
    /// only what was committed to disk survives. `rc` brings it back up.
    pub fn reboot(&self) {
        nvram::__unlink_segment();
        nvram::__reset_process_store();
        self.init();
    }

    /// A reboot where `rc` has not yet reached `nvram_init()`.
    pub fn reboot_without_init(&self) {
        nvram::__unlink_segment();
        nvram::__reset_process_store();
    }

    pub fn disk_path(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    pub fn disk_read(&self, key: &str) -> Option<Vec<u8>> {
        std::fs::read(self.disk_path(key)).ok()
    }

    pub fn disk_read_str(&self, key: &str) -> Option<String> {
        self.disk_read(key)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        nvram::__unlink_segment();
        nvram::__reset_process_store();
        // The semaphores are keyed by ftok() on the store path, so every
        // scratch directory gets its own pair. The firmware never removes
        // them - two per boot is nothing - but a suite that makes a fresh
        // directory per test orphans a pair per test and walks towards
        // SEMMNI, at which point every write on the machine starts failing.
        nvram::__remove_semaphores();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

// ---- thin wrappers so tests read like the C they replace ----

pub fn set(k: &str, v: &str) -> i32 {
    let k = CString::new(k).unwrap();
    let v = CString::new(v).unwrap();
    nvram::nvram_set(k.as_ptr(), v.as_ptr())
}

pub fn get_raw(k: &str) -> *const libc::c_char {
    let k = CString::new(k).unwrap();
    nvram::nvram_get(k.as_ptr())
}

pub fn get(k: &str) -> Option<String> {
    let p = get_raw(k);
    if p.is_null() {
        None
    } else {
        Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

pub fn get_int(k: &str) -> i32 {
    let k = CString::new(k).unwrap();
    nvram::nvram_get_int(k.as_ptr())
}

pub fn unset(k: &str) -> i32 {
    let k = CString::new(k).unwrap();
    nvram::nvram_unset(k.as_ptr())
}

/// `nvram_match` as `rc` uses it, via the inline in bcmnvram.h: a missing
/// variable never matches.
pub fn matches(k: &str, v: &str) -> i32 {
    match get(k) {
        Some(cur) if cur == v => 1,
        _ => 0,
    }
}

pub fn commit() -> i32 {
    nvram::nvram_commit()
}

pub fn clear() -> i32 {
    nvram::nvram_clear()
}

pub fn getall(buf: &mut [u8]) -> i32 {
    nvram::nvram_getall(buf.as_mut_ptr() as *mut libc::c_char, buf.len() as i32)
}

/// Split a `nvram_getall` buffer into its packed records.
pub fn parse_getall(buf: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < buf.len() {
        let end = match buf[start..].iter().position(|&b| b == 0) {
            Some(e) => start + e,
            None => break,
        };
        if end == start {
            break; // terminating empty string
        }
        out.push(String::from_utf8_lossy(&buf[start..end]).into_owned());
        start = end + 1;
    }
    out
}

pub const E_SUCCESS: i32 = 1;
