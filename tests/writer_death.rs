//! What a writer killed mid-update leaves behind.
//!
//! `SEM_UNDO` gives the lock back when a process dies holding it. The seqlock
//! counters had no equivalent: they were bumped with `fetch_add(1)` at each
//! end of an update, so a death in between left the counter odd for ever.
//! Every read of every key hashing to that slot then reported a torn read for
//! the rest of the segment's life, fell through to the disk copy, and - for a
//! key set but not yet committed, which is the normal state under deferred
//! commit - returned NULL. A live setting reading as *absent* is the worst
//! answer this store can give: `restore_defaults()` treats absent as "apply
//! the default".
//!
//! The parity is derived from the old value now rather than incremented, so
//! the next write to the slot repairs it, and the torn-read fallback repairs
//! it on the spot because it already holds the lock.

mod common;
use common::*;

#[test]
fn a_poisoned_slot_does_not_hide_an_uncommitted_value() {
    let s = Scratch::new("deathuncommitted");

    assert_eq!(set("wan_proto", "dhcp"), E_SUCCESS);
    // Deliberately no commit: there is no file to fall back to.
    assert_eq!(s.disk_read("wan_proto"), None);

    assert!(nvram::__poison_slot("wan_proto"), "expected a segment");
    s.restart_process(); // drop the cached copy, so this is a real read

    assert_eq!(
        get("wan_proto").as_deref(),
        Some("dhcp"),
        "a live uncommitted key read as absent after a writer death"
    );
}

#[test]
fn a_poisoned_slot_is_repaired_not_merely_worked_around() {
    let s = Scratch::new("deathrepair");

    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);
    assert!(nvram::__poison_slot("lan_ipaddr"));
    s.restart_process();

    // First read pays the repair.
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));

    // If the counter were still odd, this write would land on an inverted
    // parity - the window would look *even* to readers, and a concurrent read
    // would accept data being modified underneath it. Checking the value is
    // the observable half of that.
    assert_eq!(set("lan_ipaddr", "10.0.0.1"), E_SUCCESS);
    s.restart_process();
    assert_eq!(get("lan_ipaddr").as_deref(), Some("10.0.0.1"));

    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read_str("lan_ipaddr").as_deref(), Some("10.0.0.1"));
}

/// The slot is shared by hash across 4,096 counters, so a death takes out
/// every key that lands in it, not just the one being written.
#[test]
fn other_keys_sharing_the_poisoned_slot_survive_too() {
    let s = Scratch::new("deathneighbour");

    for i in 0..200 {
        assert_eq!(set(&format!("var{}", i), &format!("value{}", i)), E_SUCCESS);
    }
    for i in 0..200 {
        assert!(nvram::__poison_slot(&format!("var{}", i)));
    }
    s.restart_process();

    for i in 0..200 {
        assert_eq!(
            get(&format!("var{}", i)).as_deref(),
            Some(format!("value{}", i).as_str()),
            "var{} lost after a writer death",
            i
        );
    }
}

/// A write repairs the slot even with nothing reading it first, so a process
/// that only ever writes still puts the store back in order.
#[test]
fn a_write_alone_repairs_the_parity() {
    let s = Scratch::new("deathwrite");

    assert_eq!(set("wan_proto", "dhcp"), E_SUCCESS);
    assert!(nvram::__poison_slot("wan_proto"));

    // No read in between - straight to a write, from a process that never
    // touched the key.
    assert_eq!(set("wan_proto", "pppoe"), E_SUCCESS);
    s.restart_process();

    assert_eq!(get("wan_proto").as_deref(), Some("pppoe"));
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read_str("wan_proto").as_deref(), Some("pppoe"));
}
