//! What `nvram_commit()` does and does not hold while it writes.
//!
//! Commit runs in three phases: snapshot under the writer lock, write the
//! files holding nothing, then clear the dirty flags under the writer lock
//! again. Phase 2 used to run under the writer lock too, so a first boot -
//! ~1,100 values onto a `sync,data=journal` mount - blocked every other
//! process's `nvram_set()` for as long as it took to write them all.
//!
//! Splitting it buys that back and costs two new obligations, both tested
//! here: a value set during the write window must not be lost, and two
//! commits must not interleave their writes.

mod common;
use common::*;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn helper_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("nvram-testhelper")
}

fn spawn(s: &Scratch, args: &[&str]) -> std::process::Child {
    Command::new(helper_bin())
        .arg(s.root.to_str().unwrap())
        .arg(&s.shm)
        .args(args)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn helper")
}

/// Spawn a commit that stalls `ms` between its snapshot and its writes.
fn spawn_slow_commit(s: &Scratch, ms: u64) -> std::process::Child {
    spawn(s, &["commit-slow", &ms.to_string()])
}

/// Wait up to `limit` for `child` to exit. Returns how long it took, or
/// `None` if it was still running - in which case it is killed, so a failure
/// here reports rather than hanging the suite.
fn wait_bounded(child: &mut std::process::Child, limit: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < limit {
        if child.try_wait().expect("try_wait").is_some() {
            return Some(t0.elapsed());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Long enough that the child is reliably past its snapshot and inside the
/// window, without making the suite slow.
const SETTLE: Duration = Duration::from_millis(400);

/// The regression this file exists for. While a commit is writing, an
/// unrelated `nvram_set()` in another process must not block on it.
#[test]
fn a_commits_writes_do_not_block_another_processs_set() {
    let s = Scratch::new("commitblock");

    // Give the commit something to write, so its snapshot is not empty.
    for i in 0..50 {
        assert_eq!(set(&format!("k{}", i), &"v".repeat(64)), E_SUCCESS);
    }

    let mut child = spawn_slow_commit(&s, 3000);
    std::thread::sleep(SETTLE);
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the commit finished before we could race it; this test proved nothing"
    );

    // A third process, doing one ordinary set of a key nothing has cached, so
    // it genuinely takes the writer lock rather than being eliminated as a
    // no-op. Bounded, because the failure mode is a block with no timeout:
    // asserting on a `set` in this process would hang the suite instead of
    // failing it.
    let mut probe = spawn(&s, &["set", "probe", "1"]);
    let took = wait_bounded(&mut probe, Duration::from_millis(1500));

    let still_going = child.try_wait().expect("try_wait").is_none();
    let _ = child.wait();

    assert!(
        still_going,
        "the commit finished during the set; this test proved nothing"
    );
    match took {
        Some(d) => assert!(
            d < Duration::from_millis(1000),
            "nvram_set() took {:?} while another process was committing",
            d
        ),
        None => panic!(
            "nvram_set() was still blocked after 1.5s while another process \
             was committing - the writer lock is being held across the file \
             writes again"
        ),
    }
}

/// The obligation that splitting the phases creates. A key set *after* the
/// snapshot was taken still owes the disk a write, so its dirty flag must
/// survive the write-back.
///
/// Both values are three bytes long on purpose: commit's drift check compares
/// file size, not contents, so if the flag were wrongly cleared the later
/// commit would see a same-length file and skip it, and the new value would
/// never reach the disk at all.
#[test]
fn a_set_during_the_write_window_is_not_lost() {
    let s = Scratch::new("commitwindow");

    assert_eq!(set("wan_proto", "old"), E_SUCCESS);
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read_str("wan_proto").as_deref(), Some("old"));

    // Dirty it, then let another process start committing that value.
    assert_eq!(set("wan_proto", "mid"), E_SUCCESS);
    let mut child = spawn_slow_commit(&s, 2000);
    std::thread::sleep(SETTLE);
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the commit finished before we could race it; this test proved nothing"
    );

    // Change it again, inside the child's write window.
    assert_eq!(set("wan_proto", "new"), E_SUCCESS);
    assert!(child.wait().expect("wait").success(), "child commit failed");

    // The child wrote the value it snapshotted.
    assert_eq!(s.disk_read_str("wan_proto").as_deref(), Some("mid"));
    assert_eq!(get("wan_proto").as_deref(), Some("new"), "memory is current");

    // And ours is still owed. If the child's write-back had cleared the dirty
    // flag, this commit would find a same-length file and write nothing.
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(
        s.disk_read_str("wan_proto").as_deref(),
        Some("new"),
        "a set made during another process's commit was lost"
    );
}

/// Same again, but the change is an unset. The tombstone must still be owed.
#[test]
fn an_unset_during_the_write_window_is_not_lost() {
    let s = Scratch::new("commitwindowunset");

    assert_eq!(set("doomed", "value"), E_SUCCESS);
    let mut child = spawn_slow_commit(&s, 2000);
    std::thread::sleep(SETTLE);
    assert!(child.try_wait().expect("try_wait").is_none());

    assert_eq!(unset("doomed"), E_SUCCESS);
    assert!(child.wait().expect("wait").success());

    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read("doomed"), None, "the unset never reached disk");
}

/// A factory reset racing a commit. `nvram_clear()` must wait the commit out:
/// its writes run without the writer lock, so a clear that only took the
/// writer lock would delete the files and then watch the commit's phase 2
/// recreate them. The segment is empty by then, so nothing knows about the
/// resurrected files - until the next boot's preload turns them back into
/// variables, undoing the reset.
#[test]
fn a_clear_during_the_write_window_does_not_resurrect_files() {
    let s = Scratch::new("commitclear");

    assert_eq!(set("doomed", "value"), E_SUCCESS);
    let mut child = spawn_slow_commit(&s, 2000);
    std::thread::sleep(SETTLE);
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the commit finished before we could race it; this test proved nothing"
    );

    // Blocks on the commit lock until the child is done, then wipes.
    assert_eq!(clear(), E_SUCCESS);
    assert!(child.wait().expect("wait").success(), "child commit failed");

    assert_eq!(
        s.disk_read("doomed"),
        None,
        "a commit's write landed after clear() deleted the store"
    );

    // The boot after a factory reset must come up empty.
    s.reboot();
    assert_eq!(get("doomed"), None, "a resurrected file became a variable again");
}

/// The commit lock. Two commits must not have their writes in flight at the
/// same time, or the one holding the older snapshot can land last.
#[test]
fn two_commits_serialise_against_each_other() {
    let s = Scratch::new("commitserial");

    for i in 0..20 {
        assert_eq!(set(&format!("k{}", i), "v"), E_SUCCESS);
    }

    let pause = 700u64;
    let t0 = Instant::now();
    let mut a = spawn_slow_commit(&s, pause);
    let mut b = spawn_slow_commit(&s, pause);
    assert!(a.wait().expect("wait").success());
    assert!(b.wait().expect("wait").success());
    let elapsed = t0.elapsed();

    assert!(
        elapsed >= Duration::from_millis(pause * 2),
        "two commits overlapped ({:?} for two {}ms commits) - the commit lock \
         is not holding",
        elapsed,
        pause
    );
}

/// `SEM_UNDO` on the commit lock. A committer killed mid-write must not leave
/// the lock held, or one crash wedges every commit on the box until reboot.
#[test]
fn a_killed_committer_does_not_wedge_the_commit_lock() {
    let s = Scratch::new("commitkill");

    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);

    let mut child = spawn_slow_commit(&s, 30_000);
    std::thread::sleep(SETTLE);
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "the commit finished before we could kill it"
    );
    child.kill().expect("kill");
    let _ = child.wait();

    let t0 = Instant::now();
    assert_eq!(commit(), E_SUCCESS);
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "commit waited {:?} on a lock held by a dead process",
        t0.elapsed()
    );
    assert_eq!(s.disk_read_str("lan_ipaddr").as_deref(), Some("192.168.1.1"));
}

/// Commit still writes what it owes when nothing is racing it - the plain
/// path, phases and all.
#[test]
fn the_ordinary_path_still_persists_everything() {
    let s = Scratch::new("commitplain");

    for i in 0..200 {
        assert_eq!(set(&format!("var{}", i), &format!("value{}", i)), E_SUCCESS);
    }
    assert_eq!(commit(), E_SUCCESS);

    for i in 0..200 {
        assert_eq!(
            s.disk_read_str(&format!("var{}", i)).as_deref(),
            Some(format!("value{}", i).as_str())
        );
    }

    s.reboot();
    for i in 0..200 {
        assert_eq!(get(&format!("var{}", i)).as_deref(), Some(format!("value{}", i).as_str()));
    }
}

/// A from-scratch commit - the first boot, or a `rm -rf /nvram/*` followed by
/// `nvram commit` - writes every key. It must leave the store complete and
/// carry no temp files over.
///
/// The staging split this pins is a performance fix (one filesystem flush for
/// the batch, rather than an fsync per key), but the correctness obligation is
/// what is tested: everything lands, and nothing is left half-published.
#[test]
fn a_from_scratch_commit_rewrites_the_whole_store() {
    let s = Scratch::new("commitscratch");

    for i in 0..300 {
        assert_eq!(set(&format!("var{}", i), &format!("value{}", i)), E_SUCCESS);
    }
    assert_eq!(commit(), E_SUCCESS);

    // Wipe the disk behind the library's back, exactly as the shell does.
    for entry in std::fs::read_dir(&s.root).expect("read_dir").flatten() {
        let _ = std::fs::remove_file(entry.path());
    }
    assert_eq!(s.disk_read("var0"), None, "the wipe did not take");

    // Commit is self-healing: every key is clean, so each is stat'ed, found
    // missing, and rewritten.
    assert_eq!(commit(), E_SUCCESS);

    for i in 0..300 {
        assert_eq!(
            s.disk_read_str(&format!("var{}", i)).as_deref(),
            Some(format!("value{}", i).as_str()),
            "key var{} did not come back", i
        );
    }

    // No staged temp file may outlive the commit that made it.
    let leftovers: Vec<String> = std::fs::read_dir(&s.root)
        .expect("read_dir")
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(|n| n.to_string()))
        .filter(|n| n.starts_with(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "commit left temp files behind: {:?}", leftovers);

    // And the store survives a reboot built from those files alone.
    s.reboot();
    assert_eq!(get("var299").as_deref(), Some("value299"));
}
