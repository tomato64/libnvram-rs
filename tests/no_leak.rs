//! The regression test for the defect that started all this.
//!
//! The C library's `nvram_get()` returned `strndup()`ed memory that nothing
//! in Tomato64 ever freed, and `bcmnvram.h`'s inline wrappers - including
//! `nvram_get_int`, the one called inside loops - funnelled into it. cstats
//! leaked roughly `2 + 2H + U` allocations per minute against H LAN hosts.
//!
//! Resident set size must be flat across a large number of reads.

mod common;
use common::*;

/// Anonymous resident memory, in KB.
///
/// Deliberately *not* total RSS: the 1 MB shared arena is a tmpfs mapping and
/// lands in RssShmem, so it would otherwise show up as steady "growth" while
/// the bump allocator progressively faults it in - which is by design, and
/// bounded. RssAnon isolates the heap, which is where a leak would live.
fn rss_anon_kb() -> u64 {
    let s = std::fs::read_to_string("/proc/self/status").expect("status");
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("RssAnon:") {
            return rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .unwrap_or(0);
        }
    }
    0
}

/// Distinguish a leak from warm-up.
///
/// First-touch page faults on the 1 MB arena, and glibc's reluctance to
/// return freed memory, both show up as growth in the first batch and neither
/// is a leak. So we run two identical batches and require the *second* to be
/// flat: a real leak grows every batch equally, warm-up only the first.
fn assert_flat(label: &str, iterations: usize, mut body: impl FnMut(usize)) {
    for i in 0..iterations {
        body(i);
    }
    let after_first = rss_anon_kb();

    for i in 0..iterations {
        body(i);
    }
    let after_second = rss_anon_kb();

    let growth = after_second.saturating_sub(after_first);
    assert!(
        growth <= 64,
        "{}: heap grew {} KB during a second identical batch of {} \
         iterations (after batch 1: {} KB, after batch 2: {} KB) - that is a \
         leak, not warm-up",
        label,
        growth,
        iterations,
        after_first,
        after_second
    );
}

#[test]
fn nvram_get_does_not_leak() {
    let s = Scratch::new("leak_get");
    set("cstats_exclude", "192.168.1.5 192.168.1.6");
    assert_flat("nvram_get", 1_000_000, |_| {
        let p = get_raw("cstats_exclude");
        assert!(!p.is_null());
    });
    drop(s);
}

/// The dominant leak path in the old library: `atoi(nvram_safe_get(key))`,
/// called once per LAN host per minute by cstats.
#[test]
fn nvram_get_int_does_not_leak() {
    let s = Scratch::new("leak_int");
    set("cstats_offset", "1");
    assert_flat("nvram_get_int", 1_000_000, |_| {
        assert_eq!(get_int("cstats_offset"), 1);
    });
    drop(s);
}

/// Absent keys took the `nvram_get -> NULL -> ""` path in the old inline
/// wrapper; make sure the negative cache does not grow either.
#[test]
fn absent_key_reads_do_not_leak() {
    let s = Scratch::new("leak_absent");
    assert_flat("absent get", 1_000_000, |_| {
        assert!(get_raw("no_such_key").is_null());
    });
    drop(s);
}

/// Redundant sets are the boot-time pattern in rc: many variables re-set to
/// the value they already hold.
#[test]
fn redundant_set_does_not_leak() {
    let s = Scratch::new("leak_set");
    set("wan_proto", "dhcp");
    assert_flat("redundant set", 200_000, |_| {
        assert_eq!(set("wan_proto", "dhcp"), E_SUCCESS);
    });
    drop(s);
}

/// Real churn does allocate and free, but must not accumulate.
#[test]
fn repeated_distinct_sets_do_not_accumulate() {
    let s = Scratch::new("leak_churn");
    assert_flat("alternating set", 100_000, |i| {
        let v = if i % 2 == 0 { "a" } else { "b" };
        assert_eq!(set("flip", v), E_SUCCESS);
    });
    drop(s);
}
