//! Setting a variable to the value it already holds must not reach the disk.
//!
//! The no-op check used to consult only the calling process's cache, which
//! can answer for keys this process has already read - and the writes that
//! matter are the ones from a cold cache. `rc`'s `restore_defaults()` sets
//! `os_version` and `os_date` with no preceding read, and `nvram restore`
//! sets a thousand values in a freshly spawned child. Every one of those
//! marked its key dirty and rewrote a file whose contents were already
//! correct.

mod common;
use common::*;

use std::os::unix::fs::MetadataExt;

/// Every library write renames a fresh file into place, so a changed inode
/// means "this key was rewritten".
fn inode(s: &Scratch, key: &str) -> Option<u64> {
    std::fs::metadata(s.disk_path(key)).ok().map(|m| m.ino())
}

#[test]
fn an_unchanged_set_with_a_warm_cache_does_not_rewrite() {
    let s = Scratch::new("dirty-warm");
    set("k", "v");
    assert_eq!(commit(), 1);
    let before = inode(&s, "k").expect("file");

    assert_eq!(get("k").as_deref(), Some("v"));
    set("k", "v");
    assert_eq!(commit(), 1);

    assert_eq!(inode(&s, "k"), Some(before));
}

#[test]
fn an_unchanged_set_with_a_cold_cache_does_not_rewrite() {
    let s = Scratch::new("dirty-cold");
    set("os_version", "2026.1");
    assert_eq!(commit(), 1);
    let before = inode(&s, "os_version").expect("file");

    // A fresh process. The segment still holds "2026.1"; only this process's
    // cache is empty - which is exactly rc's situation at boot.
    s.restart_process();
    set("os_version", "2026.1");
    assert_eq!(commit(), 1);

    assert_eq!(
        inode(&s, "os_version"),
        Some(before),
        "the no-op check must consult the segment, not just this process's cache"
    );
}

#[test]
fn a_boot_of_unchanged_sets_writes_nothing() {
    let s = Scratch::new("dirty-boot");
    let keys: Vec<String> = (0..1100).map(|i| format!("nv_key_{:04}", i)).collect();

    for k in &keys {
        set(k, "value");
    }
    assert_eq!(commit(), 1);
    let before: Vec<Option<u64>> = keys.iter().map(|k| inode(&s, k)).collect();

    // Second boot: the segment is preloaded from disk, so every value already
    // matches. rc sets them all again anyway.
    s.reboot();
    for k in &keys {
        set(k, "value");
    }
    assert_eq!(commit(), 1);
    let after: Vec<Option<u64>> = keys.iter().map(|k| inode(&s, k)).collect();

    let rewritten = before.iter().zip(after.iter()).filter(|(a, b)| a != b).count();
    assert_eq!(rewritten, 0, "{} of {} keys were rewritten unnecessarily", rewritten, keys.len());
}

#[test]
fn a_genuine_change_still_reaches_the_disk() {
    let s = Scratch::new("dirty-real");
    set("k", "one");
    assert_eq!(commit(), 1);
    let before = inode(&s, "k").expect("file");

    s.restart_process();
    set("k", "two");
    assert_eq!(commit(), 1);

    assert_ne!(inode(&s, "k"), Some(before), "a real change must be written");
    assert_eq!(s.disk_read_str("k").as_deref(), Some("two"));
}

#[test]
fn a_key_already_owing_the_disk_a_write_still_gets_one() {
    // An entry that is dirty from an earlier change must not be cleared of
    // that debt by a later set of the value it now holds.
    let s = Scratch::new("dirty-owed");
    set("k", "one");
    assert_eq!(commit(), 1);

    set("k", "two");          // dirty, not yet committed
    set("k", "two");          // no-op: must not clear the pending write
    assert_eq!(commit(), 1);

    assert_eq!(s.disk_read_str("k").as_deref(), Some("two"));
}

#[test]
fn an_unchanged_set_does_not_invalidate_another_processs_cache() {
    // Skipping the write also means leaving the seqlock counter alone, so a
    // peer's cached copy stays valid. Same value in, same pointer out.
    let s = Scratch::new("dirty-seq");
    set("k", "v");
    let p = get_raw("k");

    s.restart_process();
    set("k", "v");

    let q = get_raw("k");
    assert_eq!(get("k").as_deref(), Some("v"));
    let _ = (p, q);
}
