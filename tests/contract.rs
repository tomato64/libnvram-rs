//! The contract tests: ownership, absent-vs-empty, durability, enumeration.
//!
//! Run single-threaded (`--test-threads=1`); the library is single-threaded by
//! contract and the harness swaps process-global paths between tests.

mod common;
use common::*;

#[test]
fn set_get_unset_roundtrip() {
    let s = Scratch::new("roundtrip");
    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));
    assert_eq!(unset("lan_ipaddr"), E_SUCCESS);
    assert_eq!(get("lan_ipaddr"), None);
    drop(s);
}

/// The one most likely to be got wrong: a key set to "" exists and must
/// return a non-null pointer, distinctly from a key that does not exist.
/// defaults.c has ~1,090 entries, many defaulting to "".
#[test]
fn empty_is_not_absent() {
    let s = Scratch::new("empty");

    assert!(get_raw("never_set").is_null(), "absent key must be NULL");

    assert_eq!(set("cstats_exclude", ""), E_SUCCESS);
    let p = get_raw("cstats_exclude");
    assert!(!p.is_null(), "empty value must NOT be NULL");
    assert_eq!(get("cstats_exclude").as_deref(), Some(""));

    drop(s);
}

/// A negative cache entry must be invalidated when the key appears.
#[test]
fn negative_entry_is_invalidated() {
    let s = Scratch::new("negcache");
    assert!(get_raw("later").is_null());
    assert_eq!(set("later", "here"), E_SUCCESS);
    assert_eq!(get("later").as_deref(), Some("here"));
    drop(s);
}

/// The §4 regression test. The C library overran a static 16 KB buffer via
/// `strncat` on any value larger than BUFFER_SIZE - reachable from the web UI
/// through the unvalidated OpenVPN certificate fields.
#[test]
fn large_value_roundtrip() {
    let s = Scratch::new("large");
    let big: String = std::iter::repeat("A").take(64 * 1024).collect();
    assert_eq!(set("vpns1_ca", &big), E_SUCCESS);
    assert_eq!(get("vpns1_ca").as_deref(), Some(big.as_str()));
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read("vpns1_ca").unwrap().len(), 64 * 1024);
    drop(s);
}

/// Multi-line PEM, which is how the overflow was actually reachable.
#[test]
fn multiline_pem_roundtrip() {
    let s = Scratch::new("pem");
    let mut pem = String::new();
    for _ in 0..400 {
        pem.push_str("-----BEGIN CERTIFICATE-----\n");
        pem.push_str("MIIDdzCCAl+gAwIBAgIEAgAAuTANBgkqhkiG9w0BAQUFADBaMQswCQYDVQQGEwJJ\n");
        pem.push_str("-----END CERTIFICATE-----\n");
    }
    assert!(pem.len() > 16 * 1024);
    assert_eq!(set("vpns1_ca", &pem), E_SUCCESS);
    assert_eq!(get("vpns1_ca").as_deref(), Some(pem.as_str()));
    drop(s);
}

/// A value containing '=' must survive the packed getall encoding.
#[test]
fn value_with_equals_sign() {
    let s = Scratch::new("equals");
    assert_eq!(set("script_fire", "a=b=c"), E_SUCCESS);
    let mut buf = vec![0u8; 64 * 1024];
    assert_eq!(getall(&mut buf), E_SUCCESS);
    let recs = parse_getall(&buf);
    assert!(recs.contains(&"script_fire=a=b=c".to_string()), "{:?}", recs);
    drop(s);
}

// ---------------------------------------------------------- ownership ----

/// Repeated reads of an unchanged key must return the *same* pointer: that is
/// what proves no allocation is happening per call.
#[test]
fn repeated_get_returns_stable_pointer() {
    let s = Scratch::new("stable");
    set("wan_proto", "dhcp");
    let first = get_raw("wan_proto");
    for _ in 0..10_000 {
        assert_eq!(get_raw("wan_proto"), first);
    }
    drop(s);
}

/// A pointer taken before a change must remain readable after it - the
/// retirement grace period from §7.4 of the brief.
#[test]
fn pointer_survives_one_change() {
    let s = Scratch::new("retire");
    set("lan_ipaddr", "192.168.1.1");
    let old = get_raw("lan_ipaddr");
    let old_str = unsafe { std::ffi::CStr::from_ptr(old) }.to_owned();

    set("lan_ipaddr", "10.0.0.1");
    assert_eq!(get("lan_ipaddr").as_deref(), Some("10.0.0.1"));

    // The retired buffer is still alive and still holds the old bytes.
    let still = unsafe { std::ffi::CStr::from_ptr(old) };
    assert_eq!(still, old_str.as_c_str());
    drop(s);
}

/// Setting a key to the value it already holds must not touch anything.
#[test]
fn redundant_set_is_a_noop() {
    let s = Scratch::new("noop");
    set("wan_proto", "dhcp");
    let p1 = get_raw("wan_proto");
    assert_eq!(set("wan_proto", "dhcp"), E_SUCCESS);
    assert_eq!(get_raw("wan_proto"), p1, "no-op set must not move the value");
    drop(s);
}

// -------------------------------------------------------------- ints ----

#[test]
fn get_int_parses_text() {
    let s = Scratch::new("ints");
    set("cstats_stime", "48");
    assert_eq!(get_int("cstats_stime"), 48);
    set("neg", "-7");
    assert_eq!(get_int("neg"), -7);
    set("junk", "12abc");
    assert_eq!(get_int("junk"), 12, "must behave like atoi()");
    set("empty", "");
    assert_eq!(get_int("empty"), 0);
    assert_eq!(get_int("absent"), 0);
    drop(s);
}

// -------------------------------------------------------- durability ----

/// Set without commit must be visible but must NOT reach disk. This is the
/// FreshTomato semantic the whole redesign adopts.
#[test]
fn set_is_not_durable_until_commit() {
    let s = Scratch::new("durable");

    set("wan_proto", "pppoe");
    assert_eq!(get("wan_proto").as_deref(), Some("pppoe"));
    assert!(
        s.disk_read("wan_proto").is_none(),
        "set must not write to disk"
    );

    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read("wan_proto").as_deref(), Some(&b"pppoe"[..]));
    drop(s);
}

/// After a reboot (segment gone), only committed values survive.
#[test]
fn uncommitted_values_lost_on_reboot() {
    let s = Scratch::new("reboot");

    set("kept", "yes");
    assert_eq!(commit(), E_SUCCESS);
    set("dropped", "no");

    s.reboot();

    assert_eq!(get("kept").as_deref(), Some("yes"));
    assert_eq!(get("dropped"), None, "uncommitted value must not survive");
    drop(s);
}

/// Committing an unset must remove the file.
#[test]
fn unset_then_commit_removes_file() {
    let s = Scratch::new("unlink");
    set("temp", "x");
    commit();
    assert!(s.disk_read("temp").is_some());

    unset("temp");
    commit();
    assert!(s.disk_read("temp").is_none(), "commit must unlink the file");

    s.reboot();
    assert_eq!(get("temp"), None);
    drop(s);
}

/// Values written to disk are picked up when the segment is built.
#[test]
fn preload_reads_existing_store() {
    let s = Scratch::new("preload");
    std::fs::write(s.disk_path("lan_ifname"), b"br0").unwrap();
    std::fs::write(s.disk_path("empty_one"), b"").unwrap();

    s.reboot();

    assert_eq!(get("lan_ifname").as_deref(), Some("br0"));
    assert_eq!(get("empty_one").as_deref(), Some(""));
    assert!(get_raw("empty_one") != std::ptr::null(), "empty != absent");
    drop(s);
}

// ------------------------------------------------------------- clear ----

/// `nvram_clear` must empty the segment *and* the disk. The old callers did
/// `system("rm /nvram/*")`, which under this design would leave every value
/// live in shared memory.
#[test]
fn clear_empties_both_tiers() {
    let s = Scratch::new("clear");
    set("a", "1");
    set("b", "2");
    commit();
    assert!(s.disk_read("a").is_some());

    assert_eq!(clear(), E_SUCCESS);

    assert_eq!(get("a"), None, "segment must be empty");
    assert!(s.disk_read("a").is_none(), "disk must be empty");

    s.reboot();
    assert_eq!(get("a"), None, "and must stay empty across a reboot");
    drop(s);
}

// --------------------------------------------------------- enumerate ----

#[test]
fn getall_emits_packed_records() {
    let s = Scratch::new("getall");
    set("k1", "v1");
    set("k2", "");
    set("k3", "v3");

    let mut buf = vec![0u8; 64 * 1024];
    assert_eq!(getall(&mut buf), E_SUCCESS);
    let mut recs = parse_getall(&buf);
    recs.sort();
    assert_eq!(recs, vec!["k1=v1", "k2=", "k3=v3"]);
    drop(s);
}

#[test]
fn getall_reports_failure_when_buffer_too_small() {
    let s = Scratch::new("getall_small");
    for i in 0..50 {
        set(&format!("key{}", i), "some-value-here");
    }
    let mut buf = vec![0u8; 32];
    assert_eq!(getall(&mut buf), 0, "must report overflow, not truncate silently");
    drop(s);
}

// ------------------------------------------------------------- churn ----

/// Drive enough overwrites to force arena compaction, and confirm the store
/// stays correct across it.
#[test]
fn survives_arena_compaction() {
    let s = Scratch::new("compact");

    // Each pass writes ~64 KB; the arena is 1 MB, so this compacts repeatedly.
    let big: String = std::iter::repeat('x').take(64 * 1024).collect();
    for i in 0..64 {
        assert_eq!(set("churn", &format!("{}{}", i, big)), E_SUCCESS, "pass {}", i);
        set("stable_key", "unchanged");
        assert_eq!(
            get("stable_key").as_deref(),
            Some("unchanged"),
            "compaction lost an unrelated key on pass {}",
            i
        );
    }

    assert!(get("churn").unwrap().starts_with("63"));
    drop(s);
}

/// Many distinct keys, exercising index probing.
#[test]
fn many_keys() {
    let s = Scratch::new("many");
    for i in 0..1200 {
        assert_eq!(set(&format!("var_{}", i), &format!("value_{}", i)), E_SUCCESS, "i={}", i);
    }
    for i in 0..1200 {
        assert_eq!(get(&format!("var_{}", i)).as_deref(), Some(format!("value_{}", i).as_str()));
    }

    let mut buf = vec![0u8; 1024 * 1024];
    assert_eq!(getall(&mut buf), E_SUCCESS);
    assert_eq!(parse_getall(&buf).len(), 1200);
    drop(s);
}

// -------------------------------------------------------- degradation ----

/// Documented fallback: if the shared segment cannot be created, the library
/// must still be correct - reads come from disk, sets write through, and the
/// no-free contract is unchanged. Only speed and deferred commit are lost.
#[test]
fn degrades_gracefully_without_shared_memory() {
    let root = std::env::temp_dir().join(format!("nvram-degraded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    // An shm name with an interior '/' is rejected by shm_open, which is the
    // cleanest way to simulate /dev/shm being unavailable.
    nvram::__set_paths(root.to_str().unwrap(), "/no/such/segment");
    nvram::__reset_process_store();

    assert!(nvram::__degraded(), "expected the degraded path");

    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));

    // Write-through: with no shared dirty set there is nowhere to defer to.
    assert_eq!(
        std::fs::read(root.join("lan_ipaddr")).unwrap(),
        b"192.168.1.1"
    );

    assert_eq!(get("absent"), None);
    assert_eq!(unset("lan_ipaddr"), E_SUCCESS);
    assert_eq!(get("lan_ipaddr"), None);

    nvram::__reset_process_store();
    let _ = std::fs::remove_dir_all(&root);
}

// ------------------------------------------------------- disk drift ----

/// Reproduces `rm -rf /nvram/*` followed by `nvram commit`.
///
/// The segment is authoritative, so a commit has to be able to re-establish
/// the on-disk store when something has emptied it behind the library's back.
/// Tomato's whole-buffer commit always behaved this way; a purely dirty-driven
/// commit would write nothing here, because a preloaded entry is clean.
#[test]
fn commit_restores_a_wiped_store() {
    let s = Scratch::new("wiped");

    set("lan_ipaddr", "192.168.1.1");
    set("wan_proto", "dhcp");
    set("empty_one", "");
    assert_eq!(commit(), E_SUCCESS);
    assert!(s.disk_read("lan_ipaddr").is_some());

    // Everything is clean now: committed, nothing changed since.
    assert_eq!(commit(), E_SUCCESS);

    // rm -rf /nvram/*
    for f in std::fs::read_dir(&s.root).unwrap().flatten() {
        std::fs::remove_file(f.path()).unwrap();
    }
    assert!(s.disk_read("lan_ipaddr").is_none());

    // ...but the values are still live in shared memory.
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));

    // nvram commit must put them back.
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(s.disk_read("lan_ipaddr").as_deref(), Some(&b"192.168.1.1"[..]));
    assert_eq!(s.disk_read("wan_proto").as_deref(), Some(&b"dhcp"[..]));
    assert_eq!(s.disk_read("empty_one").as_deref(), Some(&b""[..]));

    // And they survive the reboot that follows.
    s.reboot();
    assert_eq!(get("lan_ipaddr").as_deref(), Some("192.168.1.1"));
    assert_eq!(get("empty_one").as_deref(), Some(""), "empty value must survive");
    drop(s);
}

/// A truncated or edited file is repaired on the next commit too.
#[test]
fn commit_repairs_a_mangled_file() {
    let s = Scratch::new("mangled");
    set("qos_orules", "a-long-original-value");
    commit();

    std::fs::write(s.disk_path("qos_orules"), b"short").unwrap();
    assert_eq!(commit(), E_SUCCESS);
    assert_eq!(
        s.disk_read("qos_orules").as_deref(),
        Some(&b"a-long-original-value"[..])
    );
    drop(s);
}

/// Self-healing must not resurrect a key that was legitimately unset.
#[test]
fn commit_does_not_resurrect_unset_keys() {
    let s = Scratch::new("noresurrect");
    set("temp", "x");
    commit();
    unset("temp");
    commit();
    assert!(s.disk_read("temp").is_none());

    // A later, unrelated commit must leave it deleted.
    set("other", "y");
    assert_eq!(commit(), E_SUCCESS);
    assert!(s.disk_read("temp").is_none(), "unset key came back");

    s.reboot();
    assert_eq!(get("temp"), None);
    drop(s);
}
