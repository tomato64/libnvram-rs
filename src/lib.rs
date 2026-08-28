//! `libnvram.so` for Tomato64 - a shared-memory NVRAM store.
//!
//! Replaces the firmadyne-derived C library, which returned `strndup()`ed
//! memory the caller was expected to free. Nothing in Tomato64 freed it, so
//! essentially every NVRAM read in the firmware leaked. See the redesign
//! brief for the full analysis.
//!
//! # Contract
//!
//! This library implements FreshTomato's (i.e. Broadcom's) contract, which is
//! what all ~3,000 inherited call sites were written against:
//!
//! * [`nvram_get`] returns a pointer **owned by the library**. The caller
//!   never frees it. It stays valid at least until this key next changes.
//! * A null return means the key does not exist - distinctly from a pointer
//!   to `""`, which means the key exists and is empty. Callers depend on
//!   telling those apart.
//! * [`nvram_set`] is visible to every process immediately but is **not**
//!   durable. [`nvram_commit`] is the durability barrier.
//! * `E_SUCCESS` is 1 and `E_FAILURE` is 0, not the usual C convention.
//!
//! # Threading
//!
//! No Tomato64 consumer of this library is threaded (verified across cstats,
//! rstats, rc, httpd, nvram, mdu, wanuptime and dhcp6c), so the per-process
//! cache carries no lock. A threaded consumer would need one.
//!
//! # Panics
//!
//! Built with `panic = "abort"`, because unwinding across the FFI boundary is
//! undefined behaviour. Since this library is linked into pid 1, code on the
//! call paths below must be panic-free: no `unwrap` on external input, no
//! unchecked indexing.

mod cache;
mod consts;
mod disk;
mod hash;
mod paths;
mod sem;
mod shm;
mod store;

use consts::{E_FAILURE, E_SUCCESS, NVRAM_SPACE};
use std::cell::UnsafeCell;
use store::Store;

// ---------------------------------------------------------- global state --

/// Process-wide store. Single-threaded by contract (see module docs), so an
/// `UnsafeCell` is the honest representation - there is no lock to hide.
struct Global(UnsafeCell<Option<Store>>);
unsafe impl Sync for Global {}

static STORE: Global = Global(UnsafeCell::new(None));

/// Borrow the process store, building it on first use.
fn with_store<T, F: FnOnce(&mut Store) -> T>(f: F) -> T {
    let slot = unsafe { &mut *STORE.0.get() };
    if slot.is_none() {
        *slot = Some(Store::new());
    }
    match slot {
        Some(s) => f(s),
        // Unreachable: just populated above. Handled without panicking
        // because a panic here would abort pid 1.
        None => unreachable!(),
    }
}

// ------------------------------------------------------------- helpers --

/// Borrow a C string as bytes. Returns `None` for null or for anything that
/// is not usable as a key.
unsafe fn key_bytes<'a>(p: *const libc::c_char) -> Option<&'a [u8]> {
    if p.is_null() {
        return None;
    }
    let s = std::ffi::CStr::from_ptr(p).to_bytes();
    if s.is_empty() {
        return None;
    }
    Some(s)
}

unsafe fn val_bytes<'a>(p: *const libc::c_char) -> Option<&'a [u8]> {
    if p.is_null() {
        return None;
    }
    Some(std::ffi::CStr::from_ptr(p).to_bytes())
}

// ----------------------------------------------------------------- API --

/// Get a variable. Returns a borrowed pointer, or null if the key does not
/// exist. **The caller must not free the result.**
#[no_mangle]
pub extern "C" fn nvram_get(name: *const libc::c_char) -> *const libc::c_char {
    let key = match unsafe { key_bytes(name) } {
        Some(k) => k,
        None => return std::ptr::null(),
    };
    with_store(|s| s.get(key))
}

/// Alias retained for compatibility; some callers reference `_nvram_get`.
#[no_mangle]
pub extern "C" fn _nvram_get(name: *const libc::c_char) -> *const libc::c_char {
    nvram_get(name)
}

/// Get a variable, substituting `""` when it does not exist.
///
/// Note that `bcmnvram.h` defines a `static inline` of the same name which
/// shadows this in every translation unit that includes it. This exists for
/// the handful of callers that reach the library symbol directly.
#[no_mangle]
pub extern "C" fn nvram_safe_get(name: *const libc::c_char) -> *const libc::c_char {
    let p = nvram_get(name);
    if p.is_null() {
        c"".as_ptr()
    } else {
        p
    }
}

/// Get a variable parsed as an integer, or 0 if unset or unparseable.
///
/// `strtol`-based, deliberately: the C library read four raw bytes as a
/// binary `int`, which is wrong for a store holding decimal text. It was
/// never reached because libshared exports the same symbol and consumers link
/// `-lshared` first, but correctness should not depend on link order.
#[no_mangle]
pub extern "C" fn nvram_get_int(name: *const libc::c_char) -> libc::c_int {
    let key = match unsafe { key_bytes(name) } {
        Some(k) => k,
        None => return 0,
    };
    with_store(|s| {
        let p = s.get(key);
        if p.is_null() {
            return 0;
        }
        let bytes = unsafe { std::ffi::CStr::from_ptr(p) }.to_bytes();
        parse_int(bytes)
    })
}

fn parse_int(bytes: &[u8]) -> libc::c_int {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.trim(),
        Err(_) => return 0,
    };
    // Match atoi(): consume a leading integer, ignore trailing junk.
    let mut end = 0;
    let b = s.as_bytes();
    if end < b.len() && (b[end] == b'-' || b[end] == b'+') {
        end += 1;
    }
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    s[..end].parse::<libc::c_int>().unwrap_or(0)
}

/// Set a variable. Visible to every process immediately; **not durable until
/// [`nvram_commit`]**.
///
/// Returns `E_FAILURE` for a name the store cannot hold as a file - one
/// containing `/`, or starting with `.`. Those are rejected here rather than
/// at commit time, where the failure would repeat for the rest of the boot.
#[no_mangle]
pub extern "C" fn nvram_set(name: *const libc::c_char, value: *const libc::c_char) -> libc::c_int {
    let key = match unsafe { key_bytes(name) } {
        Some(k) => k,
        None => return E_FAILURE,
    };
    let val = match unsafe { val_bytes(value) } {
        Some(v) => v,
        None => return E_FAILURE,
    };
    if with_store(|s| s.set(key, val)) {
        E_SUCCESS
    } else {
        E_FAILURE
    }
}

/// Delete a variable.
///
/// Deleting a variable that does not exist succeeds - there is nothing left to
/// do, which is what the caller wanted. `E_FAILURE` means the deletion could
/// not be carried out: an unusable key, or the writer lock unavailable. The
/// result used to be discarded and this returned `E_SUCCESS` unconditionally.
#[no_mangle]
pub extern "C" fn nvram_unset(name: *const libc::c_char) -> libc::c_int {
    let key = match unsafe { key_bytes(name) } {
        Some(k) => k,
        None => return E_FAILURE,
    };
    if with_store(|s| s.unset(key)) {
        E_SUCCESS
    } else {
        E_FAILURE
    }
}

// `nvram_default_get` is deliberately **not** exported.
//
// The C library exports a two-argument `nvram_default_get(name, value)`, but
// Tomato64's libshared defines a one-argument `nvram_default_get(name)` with
// entirely different semantics - a lookup in the built-in defaults table -
// and that is the only form any header declares (`bcmnvram.h`, `defaults.h`)
// and the only form anything calls (`rc/rc/dhcp.c`). Exporting our own turned
// correctness into a question of link order: `-lshared` currently wins, but
// if it ever stopped winning, dhcp.c's fallback path would call the
// two-argument version with an uninitialised second argument and set
// `lan_ipaddr` from a garbage pointer. Nothing links against the library's
// version, so the safe move is not to have one.

/// Write every pending change to disk and flush the NVRAM filesystem.
///
/// Flushes changes made by *any* process, because the dirty set lives in the
/// shared segment.
#[no_mangle]
pub extern "C" fn nvram_commit() -> libc::c_int {
    if with_store(|s| s.commit()) {
        E_SUCCESS
    } else {
        E_FAILURE
    }
}

/// Empty the store, in memory and on disk.
#[no_mangle]
pub extern "C" fn nvram_clear() -> libc::c_int {
    if with_store(|s| s.clear()) {
        E_SUCCESS
    } else {
        E_FAILURE
    }
}

/// Reset to an empty store. Defaults are repopulated by `rc`, not here.
#[no_mangle]
pub extern "C" fn nvram_reset() -> libc::c_int {
    nvram_clear()
}

/// Fill `buf` with packed `key=value\0` records, terminated by an empty
/// string. Order is unspecified.
#[no_mangle]
pub extern "C" fn nvram_getall(buf: *mut libc::c_char, count: libc::c_int) -> libc::c_int {
    if buf.is_null() || count <= 0 {
        return E_FAILURE;
    }
    if with_store(|s| s.getall(buf, count as usize)) {
        E_SUCCESS
    } else {
        E_FAILURE
    }
}

/// Size of the store, in bytes. Matches `NVRAM_SPACE` in `bcmnvram.h`.
#[no_mangle]
pub extern "C" fn nvram_get_nvramspace() -> libc::c_int {
    NVRAM_SPACE as libc::c_int
}

/// Bring the shared store up, reading `/nvram` into it.
///
/// **Call this once per boot, from `rc`, as soon as `/nvram` is mounted and
/// before anything else needs a setting.** Until it runs, NVRAM calls still
/// work - they read and write the files directly - they are just uncached.
///
/// # Why this is not automatic
///
/// Populating the segment is the one operation whose result depends on *when*
/// it happens: it decides, once, what the store contains for every process
/// for the rest of the boot. The first version of this library did it lazily,
/// on whichever NVRAM call happened first anywhere in the system. On x86_64
/// and RPi4 `/nvram` is a mountpoint that ships in the rootfs as an empty
/// directory, so a single NVRAM touch from a hotplug handler or an init
/// script - before `mount_nvram` - published an empty store as authoritative
/// for the whole boot. The router restored its defaults on every reboot, and
/// no amount of committing helped, because the writes were landing on the
/// real partition that `rc` could no longer see.
///
/// Making the moment explicit is the fix. The belt-and-braces is in
/// `Header::store_dev`: the segment records which filesystem it was built
/// from, and a process attaching to one built from a directory that has since
/// been shadowed by a mount rejects it rather than trusting it.
///
/// Returns `E_FAILURE` if the store is not readable - which is a real signal
/// worth logging, because it means this ran too early.
///
/// Idempotent, and safe to call from more than one process; the loser of the
/// creation race attaches to the winner's segment.
#[no_mangle]
pub extern "C" fn nvram_init(_arg: *mut libc::c_void) -> libc::c_int {
    if with_store(|s| s.init()) {
        E_SUCCESS
    } else {
        E_FAILURE
    }
}

/// No-op: the mapping is released at process exit.
#[no_mangle]
pub extern "C" fn nvram_close() -> libc::c_int {
    E_SUCCESS
}

// --------------------------------------------------------- test support --

/// Test-only entry point: drop the process store so the next call rebuilds
/// it. Not part of the C ABI contract.
#[doc(hidden)]
pub fn __reset_process_store() {
    let slot = unsafe { &mut *STORE.0.get() };
    *slot = None;
}

/// Test-only: remove the shared segment.
#[doc(hidden)]
pub fn __unlink_segment() {
    shm::Segment::unlink();
}

/// Test-only: whether the shared segment is unavailable and the library has
/// fallen back to direct disk access.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn __degraded() -> bool {
    with_store(|s| s.degraded())
}

/// Test-only: point the library at a scratch store and segment.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn __set_paths(root: &str, shm: &str) {
    paths::set_paths(root, shm);
}

/// Test-only: stall a commit between its snapshot and its writes, so the suite
/// can act on the store during the window where the writer lock is
/// deliberately not held. Not part of the C ABI.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn __set_commit_pause_ms(ms: u64) {
    store::set_commit_pause_ms(ms);
}

/// Test-only: leave a key's seqlock counter odd, the way a writer killed
/// between the two halves of a segment update does. Not part of the C ABI.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn __poison_slot(name: &str) -> bool {
    with_store(|s| store::poison_slot(s, name.as_bytes()))
}

/// Test-only: destroy the semaphore sets for the current store path, so a
/// suite that gives every test its own scratch directory does not orphan a
/// pair per test.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn __remove_semaphores() {
    sem::remove_semaphores();
}

/// The longest key the store can hold. Surfaced for the test suite.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub const MAX_KEY_LEN: usize = disk::MAX_KEY_LEN;

/// Test-only: hold the writer lock for `ms`, so another process can be made
/// to block on it.
#[cfg(feature = "testing")]
#[doc(hidden)]
pub fn __hold_writer_lock_ms(ms: u64) -> bool {
    match sem::Guard::acquire() {
        Some(g) => {
            std::thread::sleep(std::time::Duration::from_millis(ms));
            drop(g);
            true
        }
        None => false,
    }
}
