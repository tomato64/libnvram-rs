//! Reproduction of the field symptom: settings reset on every reboot even
//! after an explicit `nvram commit`.
//!
//! Models a real boot at real scale rather than the handful of keys the
//! contract tests use.

mod common;
use common::*;

/// Roughly what router_defaults looks like: ~1100 keys, realistic name and
/// value lengths, a few large ones (certificates, scripts).
fn defaults() -> Vec<(String, String)> {
    let mut v = Vec::new();
    v.push(("restore_defaults".to_string(), "0".to_string()));
    v.push(("lan_ipaddr".to_string(), "192.168.1.1".to_string()));
    for i in 0..1100 {
        v.push((format!("nv_key_{:04}", i), format!("value-{}", i)));
    }
    // A handful of big ones, like OpenVPN certs and user scripts.
    for i in 0..6 {
        v.push((format!("vpn_crt_{}", i), "X".repeat(3000)));
    }
    v
}

#[test]
fn boot_then_commit_from_another_process_persists_everything() {
    let s = Scratch::new("repro-boot");

    // ---- boot 1: rc restores defaults into the segment, commits nothing ----
    let d = defaults();
    for (k, v) in &d {
        assert_eq!(set(k, v), 1, "set {} failed", k);
    }

    // ---- `nvram commit` from a separate short-lived process ----
    s.restart_process();
    assert_eq!(commit(), 1, "commit reported failure");

    // ---- what actually landed on disk? ----
    let on_disk = std::fs::read_dir(&s.root).unwrap().count();
    let missing: Vec<&str> = d
        .iter()
        .filter(|(k, _)| s.disk_read(k).is_none())
        .map(|(k, _)| k.as_str())
        .collect();

    eprintln!(
        "defaults={} on_disk={} missing={}",
        d.len(),
        on_disk,
        missing.len()
    );
    if !missing.is_empty() {
        eprintln!("first 20 missing: {:?}", &missing[..missing.len().min(20)]);
    }
    eprintln!(
        "restore_defaults on disk: {:?}",
        s.disk_read("restore_defaults")
    );

    assert!(missing.is_empty(), "{} keys never reached disk", missing.len());
}

#[test]
fn reboot_after_commit_sees_the_committed_values() {
    let s = Scratch::new("repro-reboot");

    let d = defaults();
    for (k, v) in &d {
        set(k, v);
    }
    assert_eq!(commit(), 1);

    // ---- reboot: tmpfs is gone, only disk survives ----
    s.reboot();

    assert_eq!(
        get("restore_defaults").as_deref(),
        Some("0"),
        "restore_defaults did not survive the reboot"
    );
    let lost: Vec<&str> = d
        .iter()
        .filter(|(k, v)| get(k).as_deref() != Some(v.as_str()))
        .map(|(k, _)| k.as_str())
        .collect();
    eprintln!("lost after reboot: {}", lost.len());
    if !lost.is_empty() {
        eprintln!("first 20: {:?}", &lost[..lost.len().min(20)]);
    }
    assert!(lost.is_empty(), "{} keys lost across reboot", lost.len());
}

/// Boot churn: rc does far more than 1100 sets over a boot. Every set
/// bump-allocates, so this exercises compaction under load.
#[test]
fn churn_then_commit() {
    let s = Scratch::new("repro-churn");

    let d = defaults();
    for (k, v) in &d {
        set(k, v);
    }

    // Simulate services rewriting values throughout the boot.
    for round in 0..12 {
        for (k, _) in d.iter().take(600) {
            set(k, &format!("round-{}-{}", round, k));
        }
    }
    // Put the defaults back the way restore_defaults would leave them.
    for (k, v) in &d {
        set(k, v);
    }

    s.restart_process();
    assert_eq!(commit(), 1);

    let missing: Vec<&str> = d
        .iter()
        .filter(|(k, v)| s.disk_read(k).as_deref() != Some(v.as_bytes()))
        .map(|(k, _)| k.as_str())
        .collect();
    eprintln!("after churn, wrong/missing on disk: {}", missing.len());
    if !missing.is_empty() {
        eprintln!("first 20: {:?}", &missing[..missing.len().min(20)]);
    }
    assert!(missing.is_empty(), "{} keys wrong on disk", missing.len());
}
