//! Orchestration across the three tiers.

use crate::cache::Cache;
use crate::consts::*;
use crate::disk;
use crate::hash::fnv1a;
use crate::sem;
use crate::shm::{Pending, Read, Segment};
use std::path::PathBuf;

pub struct Store {
    seg: Option<Segment>,
    cache: Cache,
    /// Counts operations performed without a segment, to throttle retries.
    detached_ops: u32,
}

impl Store {
    /// Build the process store by **attaching** to an existing segment.
    ///
    /// Deliberately never creates one. A process that finds no segment runs
    /// detached: reads go straight to disk on every call and writes go
    /// straight through, which is correct and leak-free - just slower, and
    /// without deferred commit.
    ///
    /// The moment the segment comes into existence is chosen by
    /// [`Store::init`], which `rc` calls once the store is mounted.
    pub fn new() -> Store {
        Store {
            seg: Segment::attach().ok(),
            cache: Cache::new(),
            detached_ops: 0,
        }
    }

    /// Bring the shared store up. Backs `nvram_init()`.
    ///
    /// Creates and populates the segment from `/nvram` unless a usable one
    /// already exists, and switches this process onto it. Idempotent.
    ///
    /// This is the whole point of the redesign. Populating is the one
    /// operation whose result depends on *when* it happens - it freezes "what
    /// does the store contain" for every process for the rest of the boot -
    /// so it is the one operation the caller must be able to place.
    pub fn init(&mut self) -> bool {
        if self.seg.is_some() {
            return true;
        }

        // Refuse to build a store out of a directory we cannot read. Failing
        // here is a real signal: it means the caller ran before the store was
        // ready, which is precisely the mistake this function exists to make
        // hard to blunder into.
        if !disk::store_available() || disk::store_dev().is_none() {
            return false;
        }

        if let Ok(seg) = Segment::attach() {
            self.adopt(seg);
            return true;
        }

        // Either there is no segment, or there is one this process rejected -
        // stale layout, or built from a different filesystem than the one now
        // mounted. Removing the name is safe: a process still holding the old
        // mapping keeps working against it, and every *new* attach gets the
        // one we are about to build. Reachable only when init runs after
        // something already published a bad segment, which correct placement
        // of the call avoids entirely.
        Segment::unlink();

        let created = Segment::create(|seg| {
            // Preload in full: once the segment is authoritative, "absent from
            // the segment" has to mean "does not exist", so there is no room
            // for a lazily-populated third state.
            //
            // A key that fails to land fails the whole populate. The first
            // version swallowed these (`let _ = seg.put(...)`) and published
            // the segment regardless, so a partial preload could become
            // authoritative - a router booting with an arbitrary subset of
            // its settings.
            let mut failed = false;
            let walked = disk::load_all(|k, v| {
                if seg.put(k, v, false).is_err() {
                    failed = true;
                }
            });
            if walked.is_err() || failed {
                Err(())
            } else {
                Ok(())
            }
        });

        match created {
            Ok(s) => {
                self.adopt(s);
                true
            }
            Err(()) => false,
        }
    }

    /// Switch this process from detached to segment-backed.
    ///
    /// Every cached entry must be invalidated. A detached entry recorded
    /// `slot_seen = 0`, and an untouched slot in a fresh segment also reads
    /// 0, so leaving them would make stale copies look like valid hits.
    fn adopt(&mut self, seg: Segment) {
        self.seg = Some(seg);
        self.detached_ops = 0;
        self.cache.invalidate_all();
    }

    /// Give a detached process a chance to pick up a segment created since it
    /// started. Called at the top of every operation.
    ///
    /// Throttled, because a system where `nvram_init()` is never called should
    /// not pay a failed `shm_open` on every NVRAM call. In practice this
    /// matters for one process at most - anything running before `rc` reaches
    /// `nvram_init()` - since everything spawned afterwards attaches on its
    /// first call.
    fn reattach(&mut self) {
        if self.seg.is_some() {
            return;
        }
        let n = self.detached_ops;
        self.detached_ops = n.wrapping_add(1);
        if n % REATTACH_INTERVAL != 0 {
            return;
        }
        if let Ok(seg) = Segment::attach() {
            self.adopt(seg);
        }
    }

    /// True when this process has no segment and is reading through to disk.
    /// Surfaced to the test suite; not part of the C ABI.
    #[cfg_attr(not(feature = "testing"), allow(dead_code))]
    pub fn degraded(&self) -> bool {
        self.seg.is_none()
    }

    // ------------------------------------------------------------- read --

    /// Returns a pointer owned by the library, valid until this key next
    /// changes. Null means the key does not exist.
    pub fn get(&mut self, key: &[u8]) -> *const libc::c_char {
        self.reattach();
        let seg = match &self.seg {
            Some(s) => s,
            None => return self.get_degraded(key),
        };

        let hash = fnv1a(key);
        let slot = seg.slot_of(hash);

        {
            let e = self.cache.entry(key);
            if e.valid && e.slot_seen == seg.slot_value(slot) {
                // Hit: no lock, no syscall, no copy.
                return e.as_ptr();
            }
        }

        // Miss. Read the version *before* the data: recording a version newer
        // than the bytes we actually got would make the copy stale forever.
        let mut fresh: Option<Vec<u8>> = None;
        let mut seen = 0u64;
        let mut ok = false;

        for _ in 0..MAX_SEQ_RETRIES {
            let before = seg.slot_value(slot);
            match seg.read_value(key, hash) {
                Read::Value(v) => {
                    fresh = Some(v);
                    seen = before;
                    ok = true;
                    break;
                }
                Read::Absent => {
                    fresh = None;
                    seen = before;
                    ok = true;
                    break;
                }
                Read::Torn => continue,
            }
        }

        if !ok {
            // A thousand consecutive torn reads. Either we are losing a race
            // with a writer that is compacting hard - 40 KB values rewritten
            // in a tight loop will do it, and the retry spin has no backoff -
            // or a writer died mid-update and left the counter odd.
            //
            // Falling straight through to disk here was wrong, and it is the
            // sharp end of deferred commit: an uncommitted key has no file, so
            // a live key would read back as *absent*. Callers cannot tell that
            // from a genuine unset, and `restore_defaults()` in particular
            // treats absent as "apply the default".
            //
            // Readers do not normally take the writer lock. Taking it here is
            // the point: holding it means no writer is running, so the read
            // that follows cannot be torn.
            let guard = sem::Guard::acquire();
            if guard.is_some() {
                // Holding the lock means nothing can be writing, so an odd
                // counter is not a write in progress - it is what a writer
                // killed between seq_begin and seq_end leaves behind, and it
                // is permanent until someone puts it right. Left alone, every
                // read of every key in this slot lands here for the rest of
                // the segment's life, and an uncommitted key has no file, so
                // it reports a live setting as *absent*.
                seg.repair_seq(slot);
                match seg.read_value(key, hash) {
                    Read::Value(v) => {
                        fresh = Some(v);
                        seen = seg.slot_value(slot);
                        ok = true;
                    }
                    Read::Absent => {
                        fresh = None;
                        seen = seg.slot_value(slot);
                        ok = true;
                    }
                    // Torn even after the repair. Nothing known produces
                    // this; serve the on-disk copy rather than spin forever.
                    Read::Torn => {}
                }
            }
            drop(guard);

            if !ok {
                return self.get_degraded(key);
            }
        }

        let e = self.cache.entry(key);
        e.install(fresh, seen);
        e.as_ptr()
    }

    fn get_degraded(&mut self, key: &[u8]) -> *const libc::c_char {
        let fresh = disk::read_one(key);
        let e = self.cache.entry(key);
        e.install(fresh, 0);
        // Leave the entry unusable as a cache hit. It records `slot_seen = 0`
        // because it did not come from the segment, and an untouched seqlock
        // slot also reads 0 - so an *attached* process that lands here (the
        // torn-read fallback below does) would otherwise be able to serve this
        // disk copy as a valid hit indefinitely. A detached process never
        // consults the cache at all, so this costs it nothing.
        e.valid = false;
        e.as_ptr()
    }

    // ------------------------------------------------------------ write --

    pub fn set(&mut self, key: &[u8], val: &[u8]) -> bool {
        // Refuse at the door what the disk could never hold. Accepting it here
        // would put a permanently un-writable entry in the shared segment, and
        // every nvram_commit() from then on - in every process - would report
        // failure trying to flush it.
        if !disk::valid_key(key) {
            return false;
        }
        self.reattach();
        // No-op elimination: rc re-sets a great many variables to the value
        // they already hold during boot.
        if self.value_matches(key, val) {
            return true;
        }

        let seg = match &self.seg {
            Some(s) => s,
            None => {
                // Degraded mode has no shared dirty set, so write through.
                let ok = disk::write_atomic(key, val).is_ok();
                if ok {
                    let e = self.cache.entry(key);
                    e.install(Some(val.to_vec()), 0);
                }
                return ok;
            }
        };

        let guard = match sem::Guard::acquire() {
            Some(g) => g,
            None => return false,
        };
        let res = seg.put(key, val, true);
        let hash = fnv1a(key);
        let slot = seg.slot_of(hash);
        let seen = seg.slot_value(slot);
        drop(guard);

        if res.is_err() {
            return false;
        }

        let e = self.cache.entry(key);
        e.install(Some(val.to_vec()), seen);
        true
    }

    pub fn unset(&mut self, key: &[u8]) -> bool {
        // Symmetric with `set`: such a key cannot be in the store, and a
        // tombstone for one would be just as un-writable as the value was.
        if !disk::valid_key(key) {
            return false;
        }
        self.reattach();
        let seg = match &self.seg {
            Some(s) => s,
            None => {
                let ok = disk::remove(key).is_ok();
                if ok {
                    let e = self.cache.entry(key);
                    e.install(None, 0);
                }
                return ok;
            }
        };

        let guard = match sem::Guard::acquire() {
            Some(g) => g,
            None => return false,
        };
        seg.remove(key, true);
        let hash = fnv1a(key);
        let slot = seg.slot_of(hash);
        let seen = seg.slot_value(slot);
        drop(guard);

        let e = self.cache.entry(key);
        e.install(None, seen);
        true
    }

    fn value_matches(&mut self, key: &[u8], val: &[u8]) -> bool {
        let seg = match &self.seg {
            Some(s) => s,
            None => return false,
        };
        let hash = fnv1a(key);
        let slot = seg.slot_of(hash);
        let cur = seg.slot_value(slot);

        let e = self.cache.entry(key);
        if !e.valid || e.slot_seen != cur {
            return false;
        }
        match &e.value {
            Some(v) => v.len() == val.len() + 1 && &v[..val.len()] == val,
            None => false,
        }
    }

    // ----------------------------------------------------------- commit --

    /// Flush every pending write to disk. Because the dirty set lives in the
    /// shared segment, this flushes writes made by *any* process - which is
    /// what lets `nvram restore` set values in a child that then exits, and
    /// have the parent commit them.
    ///
    /// Three phases, and the middle one deliberately holds no writer lock:
    ///
    /// 1. Under the writer lock, decide what the disk owes and copy it out.
    /// 2. Write the files, lock-free.
    /// 3. Under the writer lock again, clear the dirty flag on everything that
    ///    has not changed in the meantime.
    ///
    /// Phase 2 used to run under the writer lock as well, which meant a first
    /// boot - roughly 1,100 values onto a `sync,data=journal` mount, every one
    /// written twice and synchronously - blocked every `nvram_set()`,
    /// `nvram_unset()` and `nvram_getall()` in every other process for as long
    /// as it took. What splitting it opens up is two commits interleaving
    /// their writes; the commit lock closes that.
    pub fn commit(&mut self) -> bool {
        self.reattach();
        let seg = match &self.seg {
            Some(s) => s,
            // Degraded mode already wrote through; just flush.
            None => return disk::sync_store(),
        };

        let _commit_guard = match sem::Guard::acquire_commit() {
            Some(g) => g,
            None => return false,
        };

        // Phase 1: what does the disk owe?
        //
        // A dirty entry is owed a write outright. A clean one is checked
        // against the disk, and written when the file has gone missing or is
        // the wrong size - that is what makes commit self-healing after
        // something has messed with /nvram directly. Note the short circuit:
        // dirty entries are never stat()ed.
        let pending = {
            let guard = match sem::Guard::acquire() {
                Some(g) => g,
                None => return false,
            };
            let p = seg.commit_snapshot(|key, len| disk::differs_on_disk(key, len));
            drop(guard);
            p
        };

        // Phase 2: write, holding nothing.
        commit_pause();
        let mut all_ok = true;

        // 2a: stage every new value into its temp file, flushing none of them.
        // An fsync per key costs a device cache flush per key, and on flash
        // that is the whole cost of a commit: 1,100 keys measured 21.5s of
        // wall time against 0.35s of CPU. Nothing is published yet, so a crash
        // anywhere in here leaves the store exactly as it was, minus some temp
        // files that the next boot's load_all sweeps.
        let mut staged: Vec<(Pending, Option<PathBuf>)> = Vec::with_capacity(pending.len());
        let mut any_staged = false;
        for p in pending {
            match &p.val {
                Some(v) => match disk::stage(&p.key, v) {
                    Ok(tmp) => {
                        any_staged = true;
                        staged.push((p, Some(tmp)));
                    }
                    // Leave it dirty. The next commit will try again, which is
                    // what should happen for a transient ENOSPC or EIO.
                    Err(()) => all_ok = false,
                },
                // A deletion stages nothing; the unlink happens in 2c.
                None => staged.push((p, None)),
            }
        }

        // 2b: one flush makes every staged byte durable, in place of one per
        // key. Publishing before this would reintroduce the exact failure the
        // temp-and-rename dance exists to prevent - rename is atomic for the
        // directory entry, not for the file's contents - so if the flush
        // fails, nothing is published and everything stays dirty.
        if any_staged && !disk::sync_store() {
            for (_, tmp) in staged.iter() {
                if let Some(t) = tmp {
                    disk::discard(t);
                }
            }
            return false;
        }

        // 2c: publish. Renames and unlinks are metadata only, and the final
        // sync_store below is what makes them durable.
        let mut done: Vec<Pending> = Vec::with_capacity(staged.len());
        for (p, tmp) in staged {
            let ok = match tmp {
                Some(t) => disk::publish(&t, &p.key).is_ok(),
                None => disk::remove(&p.key).is_ok(),
            };
            if ok {
                done.push(p);
            } else {
                all_ok = false;
            }
        }

        // Phase 3: retire the flags we made good on.
        if !done.is_empty() {
            let guard = match sem::Guard::acquire() {
                Some(g) => g,
                None => return false,
            };
            seg.commit_writeback(&done);
            drop(guard);
        }

        all_ok & disk::sync_store()
    }

    // ------------------------------------------------------------ clear --

    /// Empty both the shared segment and the on-disk store.
    ///
    /// Replaces the `system("rm /nvram/*")` the callers used to do, which
    /// under this design would delete the files while leaving every value
    /// live in shared memory.
    pub fn clear(&mut self) -> bool {
        self.reattach();
        // Serialise against nvram_commit(), whose file writes deliberately run
        // without the writer lock. A commit already past its snapshot would
        // otherwise recreate its files *after* this deletes them - and a file
        // the segment does not know about becomes a variable again at the next
        // boot, resurrecting settings the clear existed to destroy. Same lock
        // order as commit (commit lock, then writer lock), so no deadlock.
        let commit_guard = sem::Guard::acquire_commit();
        if commit_guard.is_none() {
            return false;
        }
        let guard = sem::Guard::acquire();
        if guard.is_none() {
            return false;
        }
        if let Some(seg) = &self.seg {
            seg.clear();
        }
        let ok = disk::remove_all().is_ok();
        drop(guard);

        self.cache.invalidate_all();
        ok & disk::sync_store()
    }

    // -------------------------------------------------------- enumerate --

    /// Emit the whole store as packed `key=value\0` records terminated by an
    /// empty string, matching the C library byte for byte. Order is
    /// unspecified and deliberately unsorted - FreshTomato does not sort.
    pub fn getall(&mut self, buf: *mut libc::c_char, len: usize) -> bool {
        self.reattach();
        if buf.is_null() || len == 0 {
            return false;
        }

        let mut pos: usize = 0;
        let mut overflow = false;

        let mut emit = |key: &[u8], val: &[u8]| {
            if overflow {
                return;
            }
            let need = key.len() + 1 + val.len() + 1;
            if pos + need + 1 > len {
                overflow = true;
                return;
            }
            unsafe {
                let p = buf as *mut u8;
                std::ptr::copy_nonoverlapping(key.as_ptr(), p.add(pos), key.len());
                pos += key.len();
                *p.add(pos) = b'=';
                pos += 1;
                std::ptr::copy_nonoverlapping(val.as_ptr(), p.add(pos), val.len());
                pos += val.len();
                *p.add(pos) = 0;
                pos += 1;
            }
        };

        match &self.seg {
            Some(seg) => {
                // Enumerating unlocked could copy a value out from under a
                // writer. Every other entry point fails rather than proceed
                // without the lock; this one used to be the exception.
                let _guard = match sem::Guard::acquire() {
                    Some(g) => g,
                    None => return false,
                };
                seg.for_each_live(|k, v| emit(k, v));
            }
            None => {
                let _ = disk::load_all(|k, v| emit(k, v));
            }
        }

        // Terminating empty string.
        unsafe { *(buf as *mut u8).add(pos.min(len - 1)) = 0 };
        !overflow
    }
}

// ---------------------------------------------------------- test support --

/// Stall between a commit's snapshot and its writes, so the suite can act on
/// the store during the window where the writer lock is deliberately not
/// held. Compiled out of the firmware entirely.
#[cfg(feature = "testing")]
mod pause {
    use std::cell::Cell;
    thread_local! {
        pub static MS: Cell<u64> = const { Cell::new(0) };
    }
}

#[cfg(feature = "testing")]
pub fn set_commit_pause_ms(ms: u64) {
    pause::MS.with(|c| c.set(ms));
}

#[cfg(feature = "testing")]
fn commit_pause() {
    let ms = pause::MS.with(|c| c.get());
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

#[cfg(not(feature = "testing"))]
fn commit_pause() {}

/// Test-only: leave a key's seqlock counter odd, the way a writer killed
/// between `seq_begin` and `seq_end` does.
#[cfg(feature = "testing")]
pub fn poison_slot(store: &mut Store, key: &[u8]) -> bool {
    match &store.seg {
        Some(seg) => {
            seg.poison_slot(fnv1a(key));
            true
        }
        None => false,
    }
}
