//! Cross-process coherency - the properties the shared-segment design exists
//! to provide, and the ones a per-process cache design would fail.
//!
//! Each test spawns a real second process (`nvram-testhelper`) rather than
//! forking, both to avoid fork-in-a-threaded-harness hazards and because it
//! mirrors the firmware: httpd spawns the `nvram` CLI, which sets values and
//! exits, and httpd then commits.

mod common;
use common::*;

use std::path::PathBuf;
use std::process::{Command, Stdio};

fn helper_bin() -> PathBuf {
    // target/debug/deps/<test binary> -> target/debug/nvram-testhelper
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("nvram-testhelper")
}

fn helper(s: &Scratch, args: &[&str]) -> bool {
    let status = Command::new(helper_bin())
        .arg(s.root.to_str().unwrap())
        .arg(&s.shm)
        .args(args)
        .stdout(Stdio::null())
        .status()
        .expect("spawn helper");
    status.success()
}

fn helper_out(s: &Scratch, args: &[&str]) -> String {
    let out = Command::new(helper_bin())
        .arg(s.root.to_str().unwrap())
        .arg(&s.shm)
        .args(args)
        .output()
        .expect("spawn helper");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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

/// **The single most important test in the suite.** Process A caches a key,
/// process B changes it, and A's next read must return B's value - with no
/// commit and nothing written to disk.
#[test]
fn write_in_another_process_is_seen_immediately() {
    let s = Scratch::new("mp_coherent");

    set("lan_ipaddr", "192.168.1.1");
    // Read it enough times to be certain it is cached.
    for _ in 0..100 {
        assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));
    }

    assert!(helper(&s, &["set", "lan_ipaddr", "10.0.0.1"]));

    assert_eq!(
        get("lan_ipaddr").as_deref(),
        Some("10.0.0.1"),
        "cached value was not invalidated by another process's write"
    );
    assert!(
        s.disk_read("lan_ipaddr").is_none(),
        "a plain set must not have reached disk"
    );
    drop(s);
}

#[test]
fn unset_in_another_process_is_seen_immediately() {
    let s = Scratch::new("mp_unset");
    set("temp_key", "value");
    assert_eq!(get("temp_key").as_deref(), Some("value"));

    assert!(helper(&s, &["unset", "temp_key"]));

    assert_eq!(get("temp_key"), None, "deletion not observed");
    drop(s);
}

#[test]
fn write_here_is_seen_in_another_process() {
    let s = Scratch::new("mp_reverse");
    set("wan_proto", "pppoe");
    assert_eq!(helper_out(&s, &["get", "wan_proto"]), "pppoe");
    drop(s);
}

/// The config-restore flow (redesign brief §13.5): a child process performs
/// the sets and exits, and the *parent* commits. Only works because the dirty
/// set lives in shared memory.
#[test]
fn child_sets_child_exits_parent_commits() {
    let s = Scratch::new("mp_restore");

    // Stand in for `nvram restore`: a separate process writes many keys and
    // exits without committing.
    assert!(helper(&s, &["setmany", "restored_", "200"]));

    // Nothing on disk yet.
    assert!(s.disk_read("restored_0").is_none());

    // The parent - which never set any of them - commits.
    assert_eq!(commit(), E_SUCCESS);

    for i in [0usize, 99, 199] {
        assert_eq!(
            s.disk_read(&format!("restored_{}", i)).as_deref(),
            Some(format!("v{}", i).as_bytes()),
            "key {} did not reach disk",
            i
        );
    }

    // And they survive a reboot.
    s.reboot();
    assert_eq!(get("restored_150").as_deref(), Some("v150"));
    drop(s);
}

/// Concurrent writers on distinct keys must not lose updates.
#[test]
fn concurrent_writers_distinct_keys() {
    let s = Scratch::new("mp_distinct");

    let mut kids: Vec<_> = (0..4)
        .map(|i| {
            let prefix = format!("w{}_", i);
            spawn(&s, &["setmany", &prefix, "150"])
        })
        .collect();

    for k in kids.iter_mut() {
        assert!(k.wait().expect("wait").success());
    }

    s.restart_process();
    for w in 0..4 {
        for i in [0usize, 74, 149] {
            assert_eq!(
                get(&format!("w{}_{}", w, i)).as_deref(),
                Some(format!("v{}", i).as_str()),
                "lost update w{}_{}",
                w,
                i
            );
        }
    }
    drop(s);
}

/// A reader hammering a key while another process rewrites it with wildly
/// different lengths. This is the seqlock torn-read test: every observed
/// value must be one the writer actually wrote, never a mixture.
///
/// It is also the regression test for a second bug. `churn_key` is never
/// committed, so it exists only in the segment - and the reader's fallback
/// after exhausting its seqlock retries used to be "read the file", which for
/// an uncommitted key means "absent". A live key would intermittently read
/// back as `NULL`. Under sustained compaction the retries really do run out,
/// so the fallback now takes the writer lock and reads under it instead.
#[test]
fn reader_never_sees_a_torn_value() {
    let s = Scratch::new("mp_seqlock");
    set("churn_key", "S");

    let mut writer = spawn(&s, &["churn", "churn_key", "3000"]);

    let long = "L".repeat(40_000);
    let mut observations = 0usize;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);

    loop {
        match get("churn_key") {
            Some(v) => {
                assert!(
                    v == "S" || v == long,
                    "torn read: {} bytes starting {:?}",
                    v.len(),
                    &v.chars().take(8).collect::<String>()
                );
                observations += 1;
            }
            None => panic!(
                "a live key read back as absent - the reader gave up on the \
                 seqlock and fell through to a file that does not exist"
            ),
        }
        if let Ok(Some(_)) = writer.try_wait() {
            break;
        }
        if std::time::Instant::now() > deadline {
            let _ = writer.kill();
            break;
        }
    }

    assert!(observations > 100, "too few reads to be meaningful");
    drop(s);
}

/// Two processes writing the *same* key: the final value must be one of the
/// two written, never a mixture.
#[test]
fn concurrent_writers_same_key() {
    let s = Scratch::new("mp_samekey");

    let mut a = spawn(&s, &["churn", "shared_key", "400"]);
    let mut b = spawn(&s, &["churn", "shared_key", "400"]);
    assert!(a.wait().unwrap().success());
    assert!(b.wait().unwrap().success());

    s.restart_process();
    let v = get("shared_key").expect("key present");
    let long = "L".repeat(40_000);
    assert!(v == "S" || v == long, "final value is a mixture ({} bytes)", v.len());
    drop(s);
}

/// A second process starting up must attach to the existing segment rather
/// than repopulating from disk - otherwise uncommitted values would vanish.
#[test]
fn second_process_attaches_to_live_segment() {
    let s = Scratch::new("mp_attach");
    set("uncommitted", "in-memory-only");
    assert!(s.disk_read("uncommitted").is_none());

    assert_eq!(
        helper_out(&s, &["get", "uncommitted"]),
        "in-memory-only",
        "new process did not attach to the live segment"
    );
    drop(s);
}

/// A half-created segment must never be destroyed or mistaken for a version
/// mismatch.
///
/// There is a window between shm_open(O_CREAT|O_EXCL) and the creator sizing
/// and initialising the object. A process opening it in that window sees a
/// zero-length mapping with an all-zero header. Treating that as a bad
/// version - and unlinking - orphans the creator's segment and makes the next
/// process build a fresh, empty one. On a new installation, where /nvram
/// starts empty, that ends with services reading a store that has nothing in
/// it.
#[test]
fn half_created_segment_is_not_destroyed() {
    let s = Scratch::new("halfcreate");

    // Stand in for a creator that got as far as O_CREAT|O_EXCL and no further:
    // an existing but zero-length object with an all-zero header.
    let shm_path = std::path::PathBuf::from("/dev/shm").join(s.shm.trim_start_matches('/'));
    let _ = std::fs::remove_file(&shm_path);
    std::fs::File::create(&shm_path).expect("create stub segment");
    assert_eq!(std::fs::metadata(&shm_path).unwrap().len(), 0);

    // Must not fault, and must not leave the store unusable.
    let _ = get("anything");

    // Whatever it decided, the library has to keep working.
    nvram::__reset_process_store();
    let _ = std::fs::remove_file(&shm_path);
    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));
    drop(s);
}

/// Many processes starting at once against an empty store must all end up
/// sharing one segment - the new-installation boot storm.
#[test]
fn concurrent_cold_start_converges_on_one_segment() {
    let s = Scratch::new("coldstart");
    nvram::__unlink_segment();
    nvram::__reset_process_store();

    // Race a crowd of processes into creating/attaching the segment.
    let mut kids: Vec<_> = (0..8)
        .map(|i| spawn(&s, &["set", &format!("racer{}", i), "here"]))
        .collect();
    for k in kids.iter_mut() {
        assert!(k.wait().unwrap().success());
    }

    // If they had split across disconnected segments, some of these would be
    // missing from whichever one we attach to.
    s.restart_process();
    for i in 0..8 {
        assert_eq!(
            get(&format!("racer{}", i)).as_deref(),
            Some("here"),
            "racer{} lost - processes did not share one segment",
            i
        );
    }
    drop(s);
}

// ---------------------------------------------------------------------------
// Explicit bring-up, across processes.
// ---------------------------------------------------------------------------

#[test]
fn a_detached_process_reattaches_when_another_process_brings_the_store_up() {
    // This process starts before anything has called nvram_init(), so it runs
    // detached, reading and writing the files directly.
    let s = Scratch::new_uninitialised("mp-reattach");
    std::fs::write(s.root.join("lan_ipaddr"), "192.168.1.1").unwrap();

    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));
    assert!(nvram::__degraded(), "no segment yet");

    // rc reaches nvram_init(), in a different process.
    assert!(helper(&s, &["init"]), "helper init");

    // Our next calls must find it. Reattachment is throttled, so drive enough
    // operations to cross the interval rather than assuming the first one.
    for _ in 0..64 {
        let _ = get("lan_ipaddr");
        if !nvram::__degraded() {
            break;
        }
    }
    assert!(
        !nvram::__degraded(),
        "a detached process must pick up a segment created after it started"
    );

    // And it must not be serving anything cached from before the switch.
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));
    assert!(helper(&s, &["set", "lan_ipaddr", "10.0.0.1"]), "helper set");
    assert_eq!(
        get("lan_ipaddr").as_deref(),
        Some("10.0.0.1"),
        "cross-process visibility must work immediately after reattaching"
    );
}

#[test]
fn a_short_lived_process_attaches_rather_than_creating() {
    // The `nvram` CLI shape: run before rc brings the store up, and it must
    // still read correctly - just without a cache, and without publishing a
    // segment that would then be authoritative for the boot.
    let s = Scratch::new_uninitialised("mp-cli-early");
    std::fs::write(s.root.join("k"), "on-disk").unwrap();

    assert_eq!(helper_out(&s, &["get", "k"]), "on-disk");
    assert!(
        nvram::__degraded(),
        "a child running detached must not have created a segment"
    );
}
