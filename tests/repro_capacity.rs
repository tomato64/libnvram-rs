//! Does the 1 MB arena actually hold a real router's NVRAM through a boot?
//!
//! A real store is bigger than it looks: a measured Tomato64 box had **3,462**
//! files in `/nvram`, so that is the count this models - the first pass at this
//! guessed 1,100 and was a third of the truth. On top of the small keys it has
//! OpenVPN certificates, user scripts, static-DHCP lists and ndpi rule blobs,
//! and rc rewrites values thousands of times over a boot - each rewrite
//! bump-allocating a fresh copy.

mod common;
use common::*;

/// A store shaped like a real one: mostly small keys, plus the handful of
/// multi-kilobyte fields that actually exist in Tomato's defaults.
fn realistic_store() -> Vec<(String, String)> {
    let mut v = Vec::new();
    v.push(("restore_defaults".to_string(), "0".to_string()));
    for i in 0..3450 {
        v.push((format!("nv_key_{:04}", i), format!("value-{}", i)));
    }
    // OpenVPN: 2 servers + 2 clients, each with ca/cert/key/dh.
    for i in 0..4 {
        v.push((format!("vpn_server{}_ca", i), "C".repeat(2200)));
        v.push((format!("vpn_server{}_crt", i), "C".repeat(2600)));
        v.push((format!("vpn_server{}_key", i), "K".repeat(3200)));
        v.push((format!("vpn_server{}_dh", i), "D".repeat(1600)));
    }
    // User scripts, static DHCP, port forwards, ndpi rules.
    for n in ["script_init", "script_fire", "script_shut", "script_wanup"] {
        v.push((n.to_string(), "#!/bin/sh\n".repeat(400)));
    }
    v.push(("dhcpd_static".to_string(), "AA:BB:CC:DD:EE:FF<1.2.3.4<host>".repeat(300)));
    v.push(("portforward".to_string(), "1<3<1000<2000<<1.2.3.4<note>".repeat(400)));
    v
}

fn total_bytes(v: &[(String, String)]) -> usize {
    v.iter().map(|(k, val)| k.len() + val.len()).sum()
}

#[test]
fn a_realistic_store_fits_and_every_set_succeeds() {
    let s = Scratch::new("cap-fit");
    let store = realistic_store();
    eprintln!(
        "store: {} keys, {} bytes of key+value",
        store.len(),
        total_bytes(&store)
    );

    let mut failed = Vec::new();
    for (k, v) in &store {
        if set(k, v) != 1 {
            failed.push(k.as_str());
        }
    }
    eprintln!("failed sets on a cold store: {}", failed.len());
    if !failed.is_empty() {
        eprintln!("first 10: {:?}", &failed[..failed.len().min(10)]);
    }
    assert!(failed.is_empty(), "{} sets failed", failed.len());
}

#[test]
fn a_boots_worth_of_churn_does_not_start_dropping_writes() {
    let s = Scratch::new("cap-churn");
    let store = realistic_store();
    for (k, v) in &store {
        assert_eq!(set(k, v), 1);
    }

    // rc rewrites values all through the boot. Rewrite the big fields too,
    // which is what actually burns the arena.
    let mut failed = Vec::new();
    for round in 0..40 {
        for (k, v) in store.iter() {
            let churned = format!("{}#{}", v, round);
            if set(k, &churned) != 1 {
                failed.push(format!("round {} key {}", round, k));
            }
        }
        if !failed.is_empty() {
            eprintln!("first failure at round {}: {}", round, failed[0]);
            break;
        }
    }
    eprintln!("failed sets during churn: {}", failed.len());

    // Now put the defaults back, exactly as restore_defaults() would.
    let mut restore_failed = Vec::new();
    for (k, v) in &store {
        if set(k, v) != 1 {
            restore_failed.push(k.as_str());
        }
    }
    eprintln!("failed sets while restoring defaults: {}", restore_failed.len());
    if !restore_failed.is_empty() {
        eprintln!("first 10: {:?}", &restore_failed[..restore_failed.len().min(10)]);
    }

    // The decisive question: is restore_defaults=0 actually in the store?
    eprintln!("restore_defaults in memory: {:?}", get("restore_defaults"));
    assert_eq!(commit(), 1, "commit failed");
    eprintln!(
        "restore_defaults on disk: {:?}",
        s.disk_read("restore_defaults").map(|b| String::from_utf8_lossy(&b).into_owned())
    );

    let on_disk = std::fs::read_dir(&s.root).unwrap().count();
    eprintln!("keys on disk after commit: {} (expected {})", on_disk, store.len());

    assert!(failed.is_empty(), "writes were dropped during churn");
    assert!(restore_failed.is_empty(), "writes dropped while restoring defaults");
    assert_eq!(
        s.disk_read("restore_defaults").as_deref(),
        Some(b"0".as_ref()),
        "restore_defaults did not reach disk"
    );
}
