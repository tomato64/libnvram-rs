//! Names the store cannot hold.
//!
//! The C library accepted any byte string as a key and was silently broken by
//! three shapes of it. Under deferred commit the first shape stopped being
//! merely broken and became sticky: a key that reaches the shared segment but
//! cannot reach the disk is marked dirty, fails to write, and keeps its dirty
//! flag - so `nvram_commit()` reports failure for the rest of the boot, in
//! every process, over one bad `nvram set` at the CLI.

mod common;
use common::*;

const E_FAILURE: i32 = 0;

#[test]
fn a_key_containing_a_separator_is_refused() {
    let s = Scratch::new("keysep");

    assert_eq!(set("a/b", "x"), E_FAILURE, "a separator escapes the store");
    assert_eq!(get("a/b"), None, "and nothing was stored under it");

    // Not merely rejected on the way to disk: it never entered the segment,
    // so no other process can see it either.
    let mut buf = [0u8; 4096];
    assert_eq!(getall(&mut buf), E_SUCCESS);
    assert!(parse_getall(&buf).is_empty(), "store should be empty");

    drop(s);
}

#[test]
fn an_absolute_path_is_refused_too() {
    let s = Scratch::new("keyabs");
    assert_eq!(set("/etc/passwd", "x"), E_FAILURE);
    assert_eq!(set("../escape", "x"), E_FAILURE);
    assert_eq!(commit(), E_SUCCESS);

    // Nothing was written anywhere, inside the store or above it.
    let entries: Vec<_> = std::fs::read_dir(&s.root).unwrap().flatten().collect();
    assert!(entries.is_empty(), "store should be empty, holds {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>());
    assert!(!s.root.parent().unwrap().join("escape").exists());
}

#[test]
fn the_directory_names_are_refused() {
    let s = Scratch::new("keydot");
    assert_eq!(set(".", "x"), E_FAILURE);
    assert_eq!(set("..", "x"), E_FAILURE);
    drop(s);
}

/// A leading dot used to be accepted. It wrote a file that `load_all` then
/// skipped - dotfiles are how the store marks its own in-flight temporaries -
/// so the key was silently gone at the next reboot.
#[test]
fn a_dotfile_key_is_refused_rather_than_lost_at_the_next_reboot() {
    let s = Scratch::new("keyhidden");

    assert_eq!(set(".hidden", "x"), E_FAILURE);
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read(".hidden"), None, "no file should exist");

    s.reboot();
    assert_eq!(get(".hidden"), None);
}

/// The regression that motivated the check. One refused key must not leave
/// the store owing a write it can never make.
#[test]
fn a_refused_key_does_not_wedge_every_later_commit() {
    let s = Scratch::new("keywedge");

    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);
    assert_eq!(set("bad/key", "whatever"), E_FAILURE);

    for i in 0..3 {
        assert_eq!(commit(), E_SUCCESS, "commit {} should still succeed", i);
    }
    assert_eq!(s.disk_read_str("lan_ipaddr").as_deref(), Some("192.168.1.1"));

    // And a real change still gets through afterwards.
    assert_eq!(set("lan_ipaddr", "10.0.0.1"), E_SUCCESS);
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read_str("lan_ipaddr").as_deref(), Some("10.0.0.1"));
}

#[test]
fn unset_of_an_absent_key_still_succeeds() {
    let s = Scratch::new("unsetabsent");
    assert_eq!(
        unset("never_set"),
        E_SUCCESS,
        "nothing left to do is not a failure"
    );
    drop(s);
}

#[test]
fn unset_reports_failure_for_a_name_the_store_cannot_hold() {
    let s = Scratch::new("unsetbad");
    // Used to return E_SUCCESS unconditionally: the result was discarded.
    assert_eq!(unset("bad/key"), E_FAILURE);
    assert_eq!(unset(".hidden"), E_FAILURE);
    drop(s);
}

#[test]
fn unset_of_a_real_key_succeeds_and_reaches_the_disk() {
    let s = Scratch::new("unsetreal");

    assert_eq!(set("wan_proto", "dhcp"), E_SUCCESS);
    assert_eq!(commit(), E_SUCCESS);
    assert!(s.disk_read("wan_proto").is_some());

    assert_eq!(unset("wan_proto"), E_SUCCESS);
    assert_eq!(get("wan_proto"), None);
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read("wan_proto"), None);
}

/// Degraded mode - before `rc` reaches `nvram_init()` - must refuse the same
/// names, not fall through to writing the file directly.
#[test]
fn the_same_names_are_refused_while_detached() {
    let s = Scratch::new_uninitialised("keydetached");
    assert!(nvram::__degraded());

    assert_eq!(set("a/b", "x"), E_FAILURE);
    assert_eq!(set(".hidden", "x"), E_FAILURE);
    assert_eq!(set("ok", "x"), E_SUCCESS, "an ordinary key still works");
    assert_eq!(s.disk_read_str("ok").as_deref(), Some("x"));
}

/// The case that survived the first pass at key validation. A key can be a
/// legal filename and still be unwritable, because `write_atomic` writes
/// through `.tmp.<pid>.<key>` and it is *that* name which has to fit in
/// NAME_MAX. Accepting one put a permanently un-writable entry in the shared
/// segment: every `nvram_commit()` from then on, in every process, tried it
/// and reported failure.
#[test]
fn a_key_too_long_for_its_temp_file_is_refused() {
    let s = Scratch::new("keylong");

    let longest = "k".repeat(nvram::MAX_KEY_LEN);
    assert_eq!(set(&longest, "x"), E_SUCCESS, "the longest legal key must work");

    let too_long = "k".repeat(nvram::MAX_KEY_LEN + 1);
    assert_eq!(set(&too_long, "x"), E_FAILURE);
    assert_eq!(get(&too_long), None);

    // A 250-character key is a legal filename - this is the shape that fooled
    // the shape-only check.
    let plausible = "k".repeat(250);
    assert_eq!(set(&plausible, "x"), E_FAILURE);

    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read_str(&longest).as_deref(), Some("x"));
    assert_eq!(s.disk_read(&too_long), None);

    s.reboot();
    assert_eq!(get(&longest).as_deref(), Some("x"));
}

#[test]
fn an_over_long_key_does_not_wedge_every_later_commit() {
    let s = Scratch::new("keylongwedge");

    assert_eq!(set("wan_proto", "dhcp"), E_SUCCESS);
    assert_eq!(set(&"k".repeat(250), "x"), E_FAILURE);

    for i in 0..3 {
        assert_eq!(commit(), E_SUCCESS, "commit {} should still succeed", i);
    }
    assert_eq!(s.disk_read_str("wan_proto").as_deref(), Some("dhcp"));
}
