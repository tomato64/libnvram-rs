//! Tier 3: the persistent store, one file per key under `/nvram/`.
//!
//! Read once at startup to populate the shared segment, and written only by
//! `nvram_commit()`. The on-disk format is unchanged from the C library:
//! filename is the key, contents are the raw value with no trailing newline.

use crate::paths::store_root;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Temp files created mid-commit. Skipped by enumeration and swept on load.
const TMP_PREFIX: &str = ".tmp.";

pub fn root() -> PathBuf {
    PathBuf::from(store_root())
}

/// Longest key that can be written.
///
/// Bounded by the *temporary* name, not the final one: `write_atomic` writes
/// through `.tmp.<pid>.<key>`, which is what has to fit in `NAME_MAX`. Ten
/// digits covers any pid a 64-bit kernel will issue.
pub const MAX_KEY_LEN: usize = 255 - TMP_PREFIX.len() - 10 - 1;

/// True when `key` can be stored as a file, and read back again.
///
/// Four ways a key fails that, all of which the C library accepted and was
/// silently broken by:
///
/// * a path separator escapes the store entirely;
/// * a leading `.` writes a file that [`load_all`] then skips, so the key is
///   lost at the next reboot - dotfiles are how this module marks its own
///   in-flight temporaries. This covers `.` and `..` too;
/// * a name too long for the temp file it is written through;
/// * bytes that are not UTF-8, which have no filename this code can build.
///
/// Checked at the door, in `Store::set`, rather than only here: a key that
/// reaches the shared segment but cannot reach the disk is marked dirty and
/// then fails to write on *every* commit from then on, so `nvram_commit()`
/// would report failure for the rest of the boot over one bad `nvram set`.
/// The length case is the one that survived the first pass at this, and it is
/// reachable from the web UI - `nvram restore` sets keys parsed straight out
/// of an uploaded backup file, with no bound of its own.
pub fn valid_key(key: &[u8]) -> bool {
    match std::str::from_utf8(key) {
        Ok(name) => {
            !name.is_empty()
                && name.len() <= MAX_KEY_LEN
                && !name.starts_with('.')
                && !name.contains('/')
        }
        Err(_) => false,
    }
}

fn key_path(key: &[u8]) -> Option<PathBuf> {
    if !valid_key(key) {
        return None;
    }
    Some(root().join(std::str::from_utf8(key).ok()?))
}

/// Read every key currently on disk, invoking `f(key, value)` for each.
///
/// Values are sized from the file's actual length - there is deliberately no
/// fixed maximum, which is what closes the `strncat` overflow the C library
/// had on values above BUFFER_SIZE.
pub fn load_all<F: FnMut(&[u8], &[u8])>(mut f: F) -> Result<(), ()> {
    let dir = match fs::read_dir(root()) {
        Ok(d) => d,
        Err(_) => return Err(()),
    };

    for entry in dir.flatten() {
        let name = entry.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        if name.starts_with(TMP_PREFIX) || name.starts_with('.') {
            // Sweep leftovers from a commit interrupted by a crash.
            if name.starts_with(TMP_PREFIX) {
                let _ = fs::remove_file(entry.path());
            }
            continue;
        }
        match entry.file_type() {
            Ok(t) if t.is_file() => {}
            _ => continue,
        }
        if let Ok(bytes) = fs::read(entry.path()) {
            f(name.as_bytes(), &bytes);
        }
    }
    Ok(())
}

/// Read one key straight from disk. Used only in degraded mode, when the
/// shared segment is unavailable.
pub fn read_one(key: &[u8]) -> Option<Vec<u8>> {
    fs::read(key_path(key)?).ok()
}

/// Write a value atomically: temp file in the same directory, flushed, then
/// renamed.
///
/// The C library used `fopen(path, "wb")`, which truncates before writing, so
/// a power loss mid-write left the key *empty*. Rename is atomic within a
/// filesystem, so a crash leaves either the old value or the new one.
///
/// This is the one-off form, used by a detached process. `nvram_commit()`
/// goes through [`stage`] + [`publish`] instead, which buys the same
/// guarantee for a whole batch at the cost of a single flush.
pub fn write_atomic(key: &[u8], val: &[u8]) -> Result<(), ()> {
    let tmp = write_tmp(key, val, true)?;
    publish(&tmp, key)
}

/// Write `val` into a temp file for `key` **without flushing it**, returning
/// the temp path so the caller can [`publish`] it later.
///
/// Splitting the flush out is what makes a from-scratch commit bearable. An
/// `fsync` per key costs a device cache flush per key, and on flash that
/// dominates everything else a commit does: a measured 1,100-key commit took
/// 21.5s of wall time against 0.35s of CPU - ~19.5ms per key, all of it spent
/// waiting for the device. Staging every value and flushing the filesystem
/// once is the same guarantee for one flush instead of 1,100.
///
/// **The caller must flush the store (see [`sync_store`]) before publishing
/// any staged file.** Renaming a name whose data is still only in the page
/// cache reintroduces precisely the failure the temp-and-rename dance exists
/// to prevent: rename is atomic for the directory entry, not for the file's
/// contents, so a crash can leave a correctly-named empty file.
pub fn stage(key: &[u8], val: &[u8]) -> Result<PathBuf, ()> {
    write_tmp(key, val, false)
}

fn write_tmp(key: &[u8], val: &[u8], flush: bool) -> Result<PathBuf, ()> {
    let dst = key_path(key).ok_or(())?;
    let name = dst.file_name().ok_or(())?.to_owned();

    let mut tmp_name = std::ffi::OsString::from(format!("{}{}.", TMP_PREFIX, std::process::id()));
    tmp_name.push(&name);
    let tmp = root().join(tmp_name);

    let res = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(val)?;
        if flush {
            f.sync_all()?;
        }
        Ok(())
    })();

    if res.is_err() {
        let _ = fs::remove_file(&tmp);
        return Err(());
    }
    Ok(tmp)
}

/// Rename a staged temp file over its key, publishing it. See [`stage`] for
/// the flush the caller owes before calling this.
pub fn publish(tmp: &Path, key: &[u8]) -> Result<(), ()> {
    let dst = match key_path(key) {
        Some(d) => d,
        None => {
            let _ = fs::remove_file(tmp);
            return Err(());
        }
    };
    if fs::rename(tmp, &dst).is_err() {
        let _ = fs::remove_file(tmp);
        return Err(());
    }
    Ok(())
}

/// Drop a staged temp file that will not be published.
pub fn discard(tmp: &Path) {
    let _ = fs::remove_file(tmp);
}

/// True when the on-disk copy is missing, is not a regular file, or has a
/// different length than `len`.
///
/// Deliberately a stat and not a read: it costs nothing extra to compare the
/// size the stat already returned, and that catches the cases that actually
/// happen - a file deleted or truncated behind the library's back. Comparing
/// contents would mean reading every key on every commit.
///
/// Taking a length rather than the bytes is what keeps a commit's snapshot
/// small: the decision to write is made before any value is copied out of the
/// shared arena, so only the values actually being written get copied.
pub fn differs_on_disk(key: &[u8], len: usize) -> bool {
    let path = match key_path(key) {
        Some(p) => p,
        None => return false,
    };
    match fs::metadata(&path) {
        Ok(m) => !m.is_file() || m.len() != len as u64,
        Err(_) => true,
    }
}

pub fn remove(key: &[u8]) -> Result<(), ()> {
    let path = key_path(key).ok_or(())?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(()),
    }
}

/// Delete every key file. Used by `nvram_clear()`.
pub fn remove_all() -> Result<(), ()> {
    let dir = fs::read_dir(root()).map_err(|_| ())?;
    for entry in dir.flatten() {
        if let Ok(t) = entry.file_type() {
            if t.is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// Flush just the NVRAM filesystem. False if the flush failed.
///
/// The C library called global `sync()`, which flushes every mounted
/// filesystem including USB media - and returns nothing, so an I/O error at
/// flush time was indistinguishable from success.
pub fn sync_store() -> bool {
    let path = root();
    match fs::File::open(&path) {
        Ok(f) => {
            use std::os::unix::io::AsRawFd;
            unsafe { libc::syncfs(f.as_raw_fd()) == 0 }
        }
        Err(_) => false,
    }
}

/// True when the store directory is usable.
///
/// Note what this cannot tell you: an unmounted mountpoint is a perfectly
/// good directory, so this answers `true` for a `/nvram` whose partition has
/// not been mounted yet. That is why it is no longer the guard that decides
/// whether to build the segment - `nvram_init()` is. See `store_dev`.
pub fn store_available() -> bool {
    Path::new(&store_root()).is_dir()
}

/// The device number of the filesystem currently mounted on the store.
///
/// Recorded in the segment header at populate time and rechecked by every
/// process that attaches. Mounting a partition over `/nvram` changes it, so
/// a segment built from the bare mountpoint underneath is detectable as
/// stale rather than silently authoritative.
pub fn store_dev() -> Option<u64> {
    let path = std::ffi::CString::new(store_root()).ok()?;
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.st_dev as u64)
}
