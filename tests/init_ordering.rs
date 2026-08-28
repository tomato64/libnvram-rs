//! Regression suite for the failure that withdrew the first version of this
//! library, and for the explicit `nvram_init()` that replaces the lazy
//! bring-up which caused it.
//!
//! The original bug: `/nvram` is a mountpoint on x86_64 and RPi4, and it
//! ships in the rootfs as an *empty directory*. The segment was created and
//! populated by whichever NVRAM call happened first anywhere in the system,
//! guarded only by `Path::is_dir()` - which an empty mountpoint satisfies. So
//! one NVRAM touch before `mount_nvram` published an empty store as
//! authoritative for the whole boot, and did it again on every boot, since
//! /dev/shm is tmpfs. Committing never helped: the writes really did reach
//! the partition, and `rc` simply could not see them.
//!
//! Two independent defences are tested here. The first is that nothing but
//! `nvram_init()` creates a segment, so the moment is chosen rather than
//! stumbled into. The second is that the segment records the device it was
//! populated from, so a segment built from a directory that has since been
//! shadowed by a mount is *detectably* stale.
//!
//! The two directories are deliberately placed on different filesystems, so
//! `st_dev` really does change - a mount simulated with two directories on
//! one filesystem would not exercise the guard at all.

mod common;
use common::*;

use std::path::PathBuf;

/// The bare mountpoint in the rootfs, and the real partition: two directories
/// that occupy `/nvram` at different moments in a boot.
struct Mount {
    rootfs: PathBuf,
    partition: PathBuf,
    shm: String,
}

impl Mount {
    fn new(tag: &str) -> Mount {
        // Different filesystems on purpose - see the module comment.
        let rootfs = PathBuf::from(format!(
            "{}/nvram-rootfs-{}-{}",
            env!("CARGO_MANIFEST_DIR"),
            tag,
            std::process::id()
        ));
        let partition =
            std::env::temp_dir().join(format!("nvram-part-{}-{}", tag, std::process::id()));

        for d in [&rootfs, &partition] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).expect("scratch dir");
        }

        let m = Mount {
            rootfs,
            partition,
            shm: format!("/nvram-mnt-{}-{}", tag, std::process::id()),
        };
        assert_ne!(
            dev_of(&m.rootfs),
            dev_of(&m.partition),
            "this test needs the two directories on different filesystems"
        );
        nvram::__unlink_segment();
        m.unmounted();
        nvram::__unlink_segment();
        nvram::__reset_process_store();
        m
    }

    /// Before `mount_nvram`: the bare mountpoint directory in the rootfs.
    fn unmounted(&self) {
        nvram::__set_paths(self.rootfs.to_str().unwrap(), &self.shm);
    }

    /// After `mount_nvram`: the real partition.
    fn mounted(&self) {
        nvram::__set_paths(self.partition.to_str().unwrap(), &self.shm);
    }

    fn new_process(&self) {
        nvram::__reset_process_store();
    }

    fn init(&self) -> bool {
        nvram::nvram_init(std::ptr::null_mut()) == 1
    }

    fn write_partition(&self, key: &str, val: &str) {
        std::fs::write(self.partition.join(key), val).unwrap();
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        nvram::__unlink_segment();
        nvram::__reset_process_store();
        // Two directories means two ftok keys, so both need sweeping - and
        // before the directories go, because ftok needs them to exist.
        for d in [&self.rootfs, &self.partition] {
            if let Some(p) = d.to_str() {
                nvram::__set_paths(p, &self.shm);
                nvram::__remove_semaphores();
            }
        }
        let _ = std::fs::remove_dir_all(&self.rootfs);
        let _ = std::fs::remove_dir_all(&self.partition);
    }
}

fn dev_of(p: &PathBuf) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).expect("stat").dev()
}

// ---------------------------------------------------------------------------
// Defence one: only nvram_init() creates a segment.
// ---------------------------------------------------------------------------

#[test]
fn an_nvram_call_does_not_create_a_segment() {
    let m = Mount::new("no-autocreate");
    m.unmounted();

    assert!(get_raw("anything").is_null());
    set("something", "value");
    let _ = get("something");

    assert!(
        nvram::__degraded(),
        "no NVRAM call may bring the shared store up - that is nvram_init()'s job"
    );
}

#[test]
fn a_read_before_the_mount_does_not_poison_the_boot() {
    let m = Mount::new("poison");

    // A previous boot left a real store on the partition.
    m.write_partition("restore_defaults", "0");
    m.write_partition("lan_ipaddr", "10.0.0.1");

    // Something touches NVRAM before mount_nvram - a hotplug handler, an init
    // script, anything. Under the old design this one call decided what the
    // whole boot saw.
    m.unmounted();
    assert!(get_raw("restore_defaults").is_null());

    // mount_nvram runs, then rc calls nvram_init().
    m.mounted();
    assert!(m.init(), "init must succeed once the store is mounted");

    assert_eq!(
        get("restore_defaults").as_deref(),
        Some("0"),
        "the real partition must be what the boot sees"
    );
    assert_eq!(get("lan_ipaddr").as_deref(), Some("10.0.0.1"));
}

#[test]
fn writes_made_before_init_are_picked_up_by_it() {
    let m = Mount::new("premount-write");
    m.mounted();

    // Detached: no segment yet, so this writes straight through to the files.
    set("early", "value");
    assert!(nvram::__degraded());
    assert_eq!(
        std::fs::read_to_string(m.partition.join("early")).unwrap(),
        "value",
        "a detached write must be durable immediately - there is nowhere else to put it"
    );

    assert!(m.init());
    assert_eq!(
        get("early").as_deref(),
        Some("value"),
        "the preload must include what was written before it"
    );
}

// ---------------------------------------------------------------------------
// Defence two: the segment knows which filesystem it was built from.
// ---------------------------------------------------------------------------

#[test]
fn a_segment_built_before_the_mount_is_rejected_after_it() {
    let m = Mount::new("dev-guard");
    m.write_partition("lan_ipaddr", "10.0.0.1");

    // The mistake this guard exists for: init called too early, against the
    // bare mountpoint. It succeeds, and builds an empty store.
    m.unmounted();
    assert!(m.init());
    assert!(!nvram::__degraded());
    assert!(get_raw("lan_ipaddr").is_null(), "built from the wrong directory");

    // The partition is then mounted over the top.
    m.mounted();
    m.new_process();

    // A process attaching now must refuse that segment rather than trust it,
    // and fall back to reading the files - which are correct.
    assert!(
        nvram::__degraded(),
        "a segment built from a different filesystem must be rejected"
    );
    assert_eq!(
        get("lan_ipaddr").as_deref(),
        Some("10.0.0.1"),
        "detached reads must still see the real store"
    );
}

#[test]
fn init_rebuilds_a_segment_that_was_built_before_the_mount() {
    let m = Mount::new("dev-rebuild");
    m.write_partition("lan_ipaddr", "10.0.0.1");

    m.unmounted();
    assert!(m.init());

    m.mounted();
    m.new_process();
    assert!(nvram::__degraded());

    // Calling init again after the mount replaces the bad segment.
    assert!(m.init(), "init must recover from a stale segment");
    assert!(!nvram::__degraded());
    assert_eq!(get("lan_ipaddr").as_deref(), Some("10.0.0.1"));
}

// ---------------------------------------------------------------------------
// Detached behaviour and reattachment.
// ---------------------------------------------------------------------------

#[test]
fn a_detached_process_picks_up_a_segment_created_later() {
    let m = Mount::new("reattach");
    m.mounted();
    m.write_partition("k", "from-disk");

    assert_eq!(get("k").as_deref(), Some("from-disk"));
    assert!(nvram::__degraded());

    // Another process - here, an explicit init - brings the store up.
    assert!(m.init());
    assert!(!nvram::__degraded());

    // The formerly-detached view must not have gone stale across the switch.
    assert_eq!(get("k").as_deref(), Some("from-disk"));
    set("k", "changed");
    assert_eq!(get("k").as_deref(), Some("changed"));
}

#[test]
fn a_detached_read_is_never_stale() {
    let m = Mount::new("detached-fresh");
    m.mounted();

    m.write_partition("k", "one");
    assert_eq!(get("k").as_deref(), Some("one"));

    // Changed behind our back, with no segment to notice through.
    m.write_partition("k", "two");
    assert_eq!(
        get("k").as_deref(),
        Some("two"),
        "a detached process must re-read, not cache"
    );
}

#[test]
fn init_is_idempotent() {
    let m = Mount::new("idempotent");
    m.mounted();

    assert!(m.init());
    set("k", "v");
    assert!(m.init(), "a second init must be a no-op, not a rebuild");
    assert_eq!(
        get("k").as_deref(),
        Some("v"),
        "re-running init must not discard uncommitted state"
    );
}

#[test]
fn init_fails_when_the_store_is_not_there() {
    let m = Mount::new("no-store");
    nvram::__set_paths("/nonexistent-nvram-store-xyz", &m.shm);
    nvram::__reset_process_store();

    assert!(
        !m.init(),
        "init must report failure rather than build a store out of nothing"
    );
    assert!(nvram::__degraded());
}

// ---------------------------------------------------------------------------
// The whole boot, end to end.
// ---------------------------------------------------------------------------

fn router_defaults() -> Vec<(String, String)> {
    let mut v = vec![
        ("restore_defaults".to_string(), "0".to_string()),
        ("lan_ipaddr".to_string(), "192.168.1.1".to_string()),
    ];
    for i in 0..1100 {
        v.push((format!("nv_key_{:04}", i), format!("value-{}", i)));
    }
    v
}

/// `rc`'s restore_defaults(), transcribed. Returns whether it restored.
fn restore_defaults(defaults: &[(String, String)]) -> bool {
    let restoring = matches("restore_defaults", "0") == 0;
    for (k, v) in defaults {
        if restoring || get_raw(k).is_null() {
            set(k, v);
        }
    }
    // The commit rc now performs at the end of restore_defaults(). Without
    // it, deferred commit loses every default: rc has no other commit
    // anywhere on the Tomato64 boot path.
    commit();
    restoring
}

#[test]
fn two_boots_with_a_pre_mount_touch_do_not_restore_defaults_twice() {
    let m = Mount::new("full-boot");
    let d = router_defaults();

    // ---- boot 1 ----
    m.unmounted();
    let _ = get("anything"); // the pre-mount toucher
    m.mounted();
    assert!(m.init());
    assert!(restore_defaults(&d), "a fresh store must restore defaults");

    // ---- boot 2: tmpfs is gone, the same pre-mount touch happens again ----
    nvram::__unlink_segment();
    m.new_process();
    m.unmounted();
    let _ = get("anything");
    m.mounted();
    assert!(m.init());

    assert!(
        !restore_defaults(&d),
        "defaults must not be restored a second time - this is the field failure"
    );

    let lost: Vec<&str> = d
        .iter()
        .filter(|(k, v)| get(k).as_deref() != Some(v.as_str()))
        .map(|(k, _)| k.as_str())
        .collect();
    assert!(lost.is_empty(), "{} values lost across the reboot", lost.len());
}

#[test]
fn a_user_setting_saved_after_defaults_survives_the_next_boot() {
    let m = Mount::new("user-setting");
    let d = router_defaults();

    m.mounted();
    assert!(m.init());
    restore_defaults(&d);

    // The user changes something in the web UI and saves; httpd commits.
    set("lan_ipaddr", "10.0.0.1");
    set("wan_hostname", "myrouter");
    assert_eq!(commit(), 1);

    nvram::__unlink_segment();
    m.new_process();
    m.mounted();
    assert!(m.init());

    assert!(!restore_defaults(&d), "must not wipe the user's settings");
    assert_eq!(get("lan_ipaddr").as_deref(), Some("10.0.0.1"));
    assert_eq!(get("wan_hostname").as_deref(), Some("myrouter"));
}
