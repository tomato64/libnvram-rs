//! Tier 1: the authoritative live store, in POSIX shared memory.
//!
//! Layout (one contiguous mapping, never resized so it never moves):
//!
//! ```text
//!   Header        magic/version/ready, arena bookkeeping, seqlock slots
//!   IndexEntry[]  open-addressed key index -> arena offsets
//!   arena[]       variable-length key and value bytes
//! ```
//!
//! Writers serialise on the SysV semaphore (`sem::Guard`). Readers take no
//! lock at all: they use the per-key seqlock counter in `Header::slots`,
//! which does triple duty as staleness marker, write-in-progress flag and
//! torn-read detector. See the redesign brief, section 8.

use crate::consts::*;
use crate::hash::fnv1a;
use crate::paths::shm_name;
use std::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

// ------------------------------------------------------------- states ----

const ST_EMPTY: u32 = 0;
const ST_LIVE: u32 = 1;
const ST_TOMB: u32 = 2;

const FL_DIRTY: u32 = 1;

// ------------------------------------------------------------- layout ----

#[repr(C)]
pub struct IndexEntry {
    /// 0 means "never used". Non-zero plus `state` describes the slot.
    hash: AtomicU64,
    key_off: AtomicU32,
    key_len: AtomicU32,
    val_off: AtomicU32,
    val_len: AtomicU32,
    state: AtomicU32,
    flags: AtomicU32,
}

/// Every field is atomic, including the ones only ever touched under the
/// semaphore. The reason is not symmetry: reaching the plain fields needed a
/// `&mut Header` synthesised from a raw pointer, and that aliased the
/// `&Header` the same functions were already holding, which is UB that rustc
/// annotates as `noalias`. Making the fields atomic removes the `&mut`
/// entirely. The layout is unchanged, so `SHM_VERSION` stands.
///
/// `Relaxed` is right for the fields the semaphore already serialises. The
/// three that cross the lock - `ready`, `arena_seq` and the seqlock `slots` -
/// carry the ordering, and are the only ones with anything stronger.
///
/// Note what this deliberately does **not** extend to: the arena itself is
/// read and written with plain `copy_nonoverlapping`, so a reader racing a
/// writer there is formally a data race. That is the seqlock bargain - the
/// counters either side make the torn bytes detectable, and the fences stop
/// the compiler reusing a load across them. Making a 1 MB arena atomic would
/// buy standards conformance at the cost of the thing the design is for.
#[repr(C)]
pub struct Header {
    magic: AtomicU32,
    version: AtomicU32,
    /// 0 while a process is still populating the segment from disk.
    ready: AtomicU32,
    _pad0: u32,
    /// `st_dev` of the store root at the instant this segment was populated.
    ///
    /// The guard against the failure that withdrew the first version of this
    /// library. `/nvram` is a mountpoint on x86_64 and RPi4 and ships in the
    /// rootfs as an empty directory, so a segment built before `mount_nvram`
    /// is built from the wrong filesystem - and nothing about its contents
    /// says so. The device number does say so: mounting the partition
    /// changes it, so any process attaching afterwards can see at a glance
    /// that this segment describes a directory that is no longer the store.
    store_dev: AtomicU64,
    /// Odd while a compaction is moving arena contents.
    arena_seq: AtomicU64,
    /// Bumped on every mutation. Diagnostics, and whole-cache invalidation.
    global_gen: AtomicU64,
    /// Bump pointer. Writer-only, mutated under the semaphore.
    arena_used: AtomicU64,
    /// Bytes orphaned by overwrites, reclaimed by compaction.
    arena_garbage: AtomicU64,
    /// Reserved. Held a live-key count that was maintained in five places and
    /// read in none; the space stays so the layout does not change.
    _reserved: u64,
    _pad1: u64,
    slots: [AtomicU64; SLOTS],
}

const HEADER_SIZE: usize = std::mem::size_of::<Header>();
const INDEX_SIZE: usize = INDEX_SLOTS * std::mem::size_of::<IndexEntry>();
pub const SEGMENT_SIZE: usize = HEADER_SIZE + INDEX_SIZE + ARENA_SIZE;

// The layout is an ABI: two processes running different builds of this
// library map the same object. Pinned here so that changing a field is a
// compile error rather than a corrupt store, and so the reader of a diff knows
// whether SHM_VERSION has to move with it.
const _: () = {
    assert!(std::mem::size_of::<IndexEntry>() == 32);
    assert!(std::mem::align_of::<IndexEntry>() == 8);
    assert!(HEADER_SIZE == 72 + SLOTS * 8);
    assert!(std::mem::align_of::<Header>() == 8);
    // The arena must start 8-byte aligned for the atomics either side of it to
    // sit where this says they do.
    assert!((HEADER_SIZE + INDEX_SIZE) % 8 == 0);
};

// ------------------------------------------------------------ segment ----

pub struct Segment {
    base: *mut u8,
}

/// Outcome of a lock-free read attempt.
pub enum Read {
    /// Key is present; bytes copied out.
    Value(Vec<u8>),
    /// Key is definitively absent.
    Absent,
    /// A writer was active or the data failed validation. Caller retries.
    Torn,
}

impl Segment {
    // ---------------------------------------------------- construction --

    /// Attach to an existing, ready segment. **Never creates one.**
    ///
    /// This is what every NVRAM call reaches for. If it returns `Err`, the
    /// caller runs without a cache, reading through to disk - correct, just
    /// slower.
    ///
    /// Creation being absent here is the central change from the first
    /// version of this library, where the first NVRAM call in the system
    /// created *and populated* the segment from whatever `/nvram` happened to
    /// contain at that instant. On the platforms with a dedicated partition
    /// that instant could precede `mount_nvram`, and the resulting empty
    /// segment then stood as the authoritative store for the whole boot. The
    /// moment is now chosen explicitly, by [`Segment::create`].
    pub fn attach() -> Result<Segment, ()> {
        let nm = shm_name();
        let fd = unsafe { libc::shm_open(nm.as_ptr() as *const libc::c_char, libc::O_RDWR, 0o600) };
        if fd < 0 {
            return Err(());
        }
        let seg = Self::map(fd)?;
        seg.validate()?;
        Ok(seg)
    }

    /// Create and populate the segment. Called only from `nvram_init()`.
    ///
    /// `populate` must load the on-disk store into the segment, and its
    /// failure is this function's failure - a partially populated segment
    /// must never be published, because "absent from the segment" has to mean
    /// "does not exist" for the whole of its life.
    ///
    /// Loses gracefully to a concurrent creator: if another process wins the
    /// `O_EXCL` race, this attaches to theirs instead.
    pub fn create<F>(populate: F) -> Result<Segment, ()>
    where
        F: FnOnce(&Segment) -> Result<(), ()>,
    {
        let nm = shm_name();
        let name = nm.as_ptr() as *const libc::c_char;

        let fd = unsafe { libc::shm_open(name, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600) };
        if fd < 0 {
            // Someone else got there first. Wait for them to finish and use
            // theirs; whoever created it holds the writer lock until it is
            // ready, so acquiring the lock is the cheapest correct wait.
            if let Ok(seg) = Self::attach() {
                return Ok(seg);
            }
            drop(crate::sem::Guard::acquire());
            return Self::attach();
        }

        // Hold the lock across sizing and populating. A process that opens the
        // object in between would otherwise see a zero-length mapping -
        // touching it raises SIGBUS, and its all-zero header is
        // indistinguishable from a version mismatch.
        let guard = crate::sem::Guard::acquire();

        if unsafe { libc::ftruncate(fd, SEGMENT_SIZE as libc::off_t) } != 0 {
            unsafe {
                libc::close(fd);
                libc::shm_unlink(name);
            }
            return Err(());
        }

        let seg = match Self::map(fd) {
            Ok(s) => s,
            Err(()) => {
                unsafe { libc::shm_unlink(name) };
                return Err(());
            }
        };

        // Record which filesystem we are about to read, before reading it.
        let dev = match crate::disk::store_dev() {
            Some(d) => d,
            None => {
                drop(guard);
                unsafe { libc::shm_unlink(name) };
                return Err(());
            }
        };

        // ftruncate zero-fills, so the index and arena already read empty.
        // These are ordered against every other process by the release store
        // to `ready` below, which is why plain `Relaxed` is enough here.
        {
            let h = seg.header();
            h.magic.store(SHM_MAGIC, Ordering::Relaxed);
            h.version.store(SHM_VERSION, Ordering::Relaxed);
            h.store_dev.store(dev, Ordering::Relaxed);
            h.arena_used.store(0, Ordering::Relaxed);
            h.arena_garbage.store(0, Ordering::Relaxed);
        }

        if populate(&seg).is_err() {
            drop(guard);
            unsafe { libc::shm_unlink(name) };
            return Err(());
        }

        // Release-store last: everything above must be visible before any
        // other process may conclude the segment is usable, and before the
        // lock is dropped.
        seg.header().ready.store(1, Ordering::Release);
        drop(guard);
        Ok(seg)
    }

    fn map(fd: libc::c_int) -> Result<Segment, ()> {
        // Mapping a shorter object than we ask for succeeds, but touching the
        // pages past its end raises SIGBUS, so confirm the size first.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let sized = unsafe { libc::fstat(fd, &mut st) } == 0 && st.st_size as usize >= SEGMENT_SIZE;
        if !sized {
            unsafe { libc::close(fd) };
            return Err(());
        }

        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                SEGMENT_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd) };

        if base == libc::MAP_FAILED {
            return Err(());
        }
        Ok(Segment {
            base: base as *mut u8,
        })
    }

    /// Is this segment usable by us? Ready, our layout, and describing the
    /// filesystem that is mounted on the store right now.
    ///
    /// Returns `Err` without touching the segment. The mapping is released by
    /// `Drop` when the caller discards it, and the shared object is left
    /// alone: the first version of this library unlinked here, and that
    /// produced exactly the bug it was trying to avoid - a process arriving
    /// mid-creation read an all-zero header, called it a version mismatch,
    /// and removed a segment its creator was still filling.
    fn validate(&self) -> Result<(), ()> {
        let h = self.header();

        if h.ready.load(Ordering::Acquire) == 0 {
            // Still being populated, or its creator died. Block on the lock
            // the creator holds, then look again.
            drop(crate::sem::Guard::acquire());
            if h.ready.load(Ordering::Acquire) == 0 {
                return Err(());
            }
        }

        // Only now is the rest of the header meaningful.
        if h.magic.load(Ordering::Relaxed) != SHM_MAGIC
            || h.version.load(Ordering::Relaxed) != SHM_VERSION
        {
            return Err(());
        }

        match crate::disk::store_dev() {
            Some(dev) if dev == h.store_dev.load(Ordering::Relaxed) => Ok(()),
            // Either the store is unreadable, or - the case this exists for -
            // it now lives on a different filesystem than the one this
            // segment was built from. The segment describes a directory that
            // has since been shadowed by a mount. Refuse it.
            _ => Err(()),
        }
    }

    /// Remove the segment so the next caller recreates it from disk.
    pub fn unlink() {
        let nm = shm_name();
        unsafe { libc::shm_unlink(nm.as_ptr() as *const libc::c_char) };
    }

    // -------------------------------------------------------- accessors --

    fn header(&self) -> &Header {
        unsafe { &*(self.base as *const Header) }
    }

    fn index(&self) -> &[IndexEntry] {
        unsafe {
            std::slice::from_raw_parts(
                self.base.add(HEADER_SIZE) as *const IndexEntry,
                INDEX_SLOTS,
            )
        }
    }

    fn arena_ptr(&self) -> *mut u8 {
        unsafe { self.base.add(HEADER_SIZE + INDEX_SIZE) }
    }

    pub fn slot_of(&self, hash: u64) -> usize {
        (hash & (SLOTS as u64 - 1)) as usize
    }

    pub fn slot_value(&self, slot: usize) -> u64 {
        self.header().slots[slot].load(Ordering::Acquire)
    }

    // ----------------------------------------------------- arena access --

    /// Copy `len` bytes at `off` out of the arena, refusing out-of-range
    /// requests. A torn read can produce nonsense offsets, so this is the
    /// bounds check that keeps a concurrent update from walking off the end.
    fn arena_slice(&self, off: u32, len: u32) -> Option<Vec<u8>> {
        let off = off as usize;
        let len = len as usize;
        if off > ARENA_SIZE || len > ARENA_SIZE || off + len > ARENA_SIZE {
            return None;
        }
        let mut out = vec![0u8; len];
        unsafe {
            std::ptr::copy_nonoverlapping(self.arena_ptr().add(off), out.as_mut_ptr(), len);
        }
        Some(out)
    }

    fn arena_eq(&self, off: u32, len: u32, want: &[u8]) -> bool {
        let off = off as usize;
        let len = len as usize;
        if len != want.len() || off > ARENA_SIZE || off + len > ARENA_SIZE {
            return false;
        }
        let have = unsafe { std::slice::from_raw_parts(self.arena_ptr().add(off), len) };
        have == want
    }

    /// Bump-allocate. Caller must hold the semaphore.
    fn arena_alloc(&self, bytes: &[u8]) -> Option<u32> {
        let h = self.header();
        let used = h.arena_used.load(Ordering::Relaxed) as usize;
        let need = bytes.len();
        if used + need > ARENA_SIZE {
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.arena_ptr().add(used), need);
        }
        h.arena_used
            .store((used + need) as u64, Ordering::Relaxed);
        Some(used as u32)
    }

    // ------------------------------------------------------ index probe --

    /// Locate `key`. Returns `(index, found)` where `found` means the slot
    /// holds this key (LIVE or TOMB). When not found, `index` is a slot that
    /// may be claimed, if one was available.
    fn probe(&self, key: &[u8], hash: u64) -> Option<(usize, bool)> {
        let idx = self.index();
        let start = (hash % INDEX_SLOTS as u64) as usize;
        let mut free: Option<usize> = None;

        for j in 0..MAX_PROBES {
            let i = (start + j) % INDEX_SLOTS;
            let e = &idx[i];
            let state = e.state.load(Ordering::Acquire);

            if state == ST_EMPTY {
                return Some((free.unwrap_or(i), false));
            }

            if e.hash.load(Ordering::Acquire) == hash
                && self.arena_eq(
                    e.key_off.load(Ordering::Acquire),
                    e.key_len.load(Ordering::Acquire),
                    key,
                )
            {
                return Some((i, true));
            }

            // A tombstone can be reclaimed, but only once its pending unlink
            // has been committed - otherwise we lose the "delete this file"
            // record.
            if state == ST_TOMB
                && free.is_none()
                && (e.flags.load(Ordering::Acquire) & FL_DIRTY) == 0
            {
                free = Some(i);
            }
        }

        free.map(|i| (i, false))
    }

    // --------------------------------------------------------- read path --

    /// Lock-free read. Returns `Read::Torn` if a writer was active, in which
    /// case the caller retries.
    pub fn read_value(&self, key: &[u8], hash: u64) -> Read {
        let slot = self.slot_of(hash);
        let h = self.header();

        let s1 = h.slots[slot].load(Ordering::Acquire);
        let a1 = h.arena_seq.load(Ordering::Acquire);
        if s1 & 1 == 1 || a1 & 1 == 1 {
            return Read::Torn;
        }
        fence(Ordering::Acquire);

        let found = self.probe(key, hash);
        let out = match found {
            Some((i, true)) => {
                let e = &self.index()[i];
                if e.state.load(Ordering::Acquire) == ST_LIVE {
                    match self.arena_slice(
                        e.val_off.load(Ordering::Acquire),
                        e.val_len.load(Ordering::Acquire),
                    ) {
                        Some(v) => Read::Value(v),
                        // Offsets out of range: we raced a writer.
                        None => return Read::Torn,
                    }
                } else {
                    Read::Absent
                }
            }
            _ => Read::Absent,
        };

        fence(Ordering::Acquire);
        let s2 = h.slots[slot].load(Ordering::Acquire);
        let a2 = h.arena_seq.load(Ordering::Acquire);
        if s1 != s2 || a1 != a2 {
            return Read::Torn;
        }

        out
    }

    // -------------------------------------------------------- write path --

    /// Next odd value: always odd, always strictly larger.
    ///
    /// Deriving the parity from the old value rather than incrementing it is
    /// what makes a counter self-repairing. Both of these used to be a plain
    /// `fetch_add(1)`, so a writer killed between begin and end left the
    /// counter **odd for ever**: every read of every key in that slot
    /// reported a torn read for the rest of the segment's life, and - the
    /// dangerous half - the parity was inverted, so the *next* writer's
    /// window looked even and readers accepted data being modified underneath
    /// them. `SEM_UNDO` released the lock; nothing released the counter.
    fn next_odd(v: u64) -> u64 {
        (v + 1) | 1
    }

    /// Next even value: always even, always strictly larger.
    fn next_even(v: u64) -> u64 {
        (v | 1) + 1
    }

    /// Begin a write to `slot`: make the counter odd so readers back off.
    fn seq_begin(&self, slot: usize) {
        let s = &self.header().slots[slot];
        let v = s.load(Ordering::Relaxed);
        s.store(Self::next_odd(v), Ordering::Release);
        fence(Ordering::Release);
    }

    /// End a write: make the counter even again and record the mutation.
    fn seq_end(&self, slot: usize) {
        fence(Ordering::Release);
        let s = &self.header().slots[slot];
        let v = s.load(Ordering::Relaxed);
        s.store(Self::next_even(v), Ordering::Release);
        self.header().global_gen.fetch_add(1, Ordering::AcqRel);
    }

    /// Begin a compaction: the arena counter covers every slot at once.
    fn arena_begin(&self) {
        let a = &self.header().arena_seq;
        let v = a.load(Ordering::Relaxed);
        a.store(Self::next_odd(v), Ordering::Release);
        fence(Ordering::Release);
    }

    fn arena_end(&self) {
        fence(Ordering::Release);
        let a = &self.header().arena_seq;
        let v = a.load(Ordering::Relaxed);
        a.store(Self::next_even(v), Ordering::Release);
    }

    /// Force this slot's counter and the arena counter back to even, and say
    /// whether anything needed it. **Caller must hold the writer lock.**
    ///
    /// That is the whole argument: under the lock nothing can be writing, so
    /// an odd counter is not a write in progress. It is the residue of a
    /// writer that died mid-update, and left alone it is permanent - the
    /// reader would fall through to the disk copy for ever, which for an
    /// uncommitted key means reporting a live setting as *absent*.
    pub fn repair_seq(&self, slot: usize) -> bool {
        let mut repaired = false;
        for c in [&self.header().slots[slot], &self.header().arena_seq] {
            let v = c.load(Ordering::Acquire);
            if v & 1 == 1 {
                c.store(Self::next_even(v), Ordering::Release);
                repaired = true;
            }
        }
        repaired
    }

    /// Test-only: leave a slot's counter odd, the way a writer killed between
    /// `seq_begin` and `seq_end` does.
    #[cfg(feature = "testing")]
    pub fn poison_slot(&self, hash: u64) {
        let s = &self.header().slots[self.slot_of(hash)];
        let v = s.load(Ordering::Relaxed);
        s.store(v | 1, Ordering::Release);
    }

    /// Insert or overwrite. Caller must hold the semaphore.
    ///
    /// `dirty` marks the key as needing a disk write at the next commit;
    /// preload passes `false` because the value already matches disk.
    pub fn put(&self, key: &[u8], val: &[u8], dirty: bool) -> Result<(), ()> {
        let hash = fnv1a(key);
        let slot = self.slot_of(hash);

        // Nothing to do if the store already holds exactly this value.
        //
        // This has to be tested against the *segment*, not against the calling
        // process's cache. The cache can only answer for keys this process has
        // already read, and the writes that matter most are the ones from a
        // cold cache: rc's restore_defaults() sets os_version and os_date with
        // no preceding read, and `nvram restore` sets a thousand values in a
        // freshly spawned child. Every one of those was marking its key dirty
        // and rewriting a file whose contents were already correct.
        //
        // Returning early also leaves the seqlock counter alone, which is
        // right - no reader's cached copy has been invalidated, because
        // nothing changed. An already-dirty entry stays dirty: it still owes
        // the disk a write from whatever change made it dirty in the first
        // place.
        if let Some((i, true)) = self.probe(key, hash) {
            let e = &self.index()[i];
            if e.state.load(Ordering::Acquire) == ST_LIVE
                && self.arena_eq(
                    e.val_off.load(Ordering::Acquire),
                    e.val_len.load(Ordering::Acquire),
                    val,
                )
            {
                return Ok(());
            }
        }

        // Reserve space before disturbing readers, compacting if needed.
        // Note this may move everything, so the probe below must come after.
        self.ensure_space(key.len() + val.len())?;

        let (i, found) = self.probe(key, hash).ok_or(())?;

        self.seq_begin(slot);
        let res = self.put_locked(i, found, key, val, hash, dirty);
        self.seq_end(slot);
        res
    }

    fn put_locked(
        &self,
        i: usize,
        found: bool,
        key: &[u8],
        val: &[u8],
        hash: u64,
        dirty: bool,
    ) -> Result<(), ()> {
        let e = &self.index()[i];
        let h = self.header();

        let val_off = self.arena_alloc(val).ok_or(())?;

        if found {
            // Key bytes are already in the arena and never change; only the
            // value moves, so the old value becomes garbage.
            if e.state.load(Ordering::Acquire) == ST_LIVE {
                h.arena_garbage
                    .fetch_add(e.val_len.load(Ordering::Acquire) as u64, Ordering::Relaxed);
            }
        } else {
            let key_off = self.arena_alloc(key).ok_or(())?;
            e.key_off.store(key_off, Ordering::Release);
            e.key_len.store(key.len() as u32, Ordering::Release);
            e.hash.store(hash, Ordering::Release);
        }

        // Length first, and zeroed. These are two stores, so a writer killed
        // between them leaves the entry describing the *new* offset with the
        // *old* length - a value assembled from two different writes. Zeroing
        // first makes the worst case a key that reads empty instead.
        e.val_len.store(0, Ordering::Release);
        e.val_off.store(val_off, Ordering::Release);
        e.val_len.store(val.len() as u32, Ordering::Release);
        if dirty {
            e.flags.fetch_or(FL_DIRTY, Ordering::AcqRel);
        }
        e.state.store(ST_LIVE, Ordering::Release);
        Ok(())
    }

    /// Delete a key. Caller must hold the semaphore.
    pub fn remove(&self, key: &[u8], dirty: bool) -> bool {
        let hash = fnv1a(key);
        let slot = self.slot_of(hash);

        let (i, found) = match self.probe(key, hash) {
            Some(v) => v,
            None => return false,
        };
        if !found {
            return false;
        }

        let e = &self.index()[i];
        if e.state.load(Ordering::Acquire) != ST_LIVE {
            return false;
        }

        self.seq_begin(slot);
        self.header()
            .arena_garbage
            .fetch_add(e.val_len.load(Ordering::Acquire) as u64, Ordering::Relaxed);
        e.val_len.store(0, Ordering::Release);
        if dirty {
            e.flags.fetch_or(FL_DIRTY, Ordering::AcqRel);
        }
        e.state.store(ST_TOMB, Ordering::Release);
        self.seq_end(slot);
        true
    }

    // ------------------------------------------------------ housekeeping --

    fn ensure_space(&self, need: usize) -> Result<(), ()> {
        let h = self.header();
        if h.arena_used.load(Ordering::Relaxed) as usize + need <= ARENA_SIZE {
            // Compact opportunistically once garbage is worth reclaiming.
            if h.arena_garbage.load(Ordering::Relaxed) as usize
                > ARENA_SIZE / COMPACT_GARBAGE_DIVISOR
            {
                self.compact();
            }
            return Ok(());
        }
        self.compact();
        if h.arena_used.load(Ordering::Relaxed) as usize + need > ARENA_SIZE {
            return Err(());
        }
        Ok(())
    }

    /// Rebuild the arena and index, dropping garbage and clean tombstones.
    ///
    /// Safe with respect to outstanding caller pointers precisely because no
    /// caller pointer aims into the arena - tier 2 hands out private copies.
    fn compact(&self) {
        // Snapshot everything worth keeping before touching the arena.
        struct Keep {
            key: Vec<u8>,
            val: Option<Vec<u8>>,
            flags: u32,
        }
        let mut keep: Vec<Keep> = Vec::new();

        for e in self.index().iter() {
            let state = e.state.load(Ordering::Acquire);
            if state == ST_EMPTY {
                continue;
            }
            let flags = e.flags.load(Ordering::Acquire);
            if state == ST_TOMB && (flags & FL_DIRTY) == 0 {
                continue; // committed deletion: nothing left to remember
            }
            let key = match self.arena_slice(
                e.key_off.load(Ordering::Acquire),
                e.key_len.load(Ordering::Acquire),
            ) {
                Some(k) => k,
                None => continue,
            };
            let val = if state == ST_LIVE {
                self.arena_slice(
                    e.val_off.load(Ordering::Acquire),
                    e.val_len.load(Ordering::Acquire),
                )
            } else {
                None
            };
            keep.push(Keep { key, val, flags });
        }

        self.arena_begin();

        // Wipe index and arena bookkeeping, then re-insert.
        for e in self.index().iter() {
            e.state.store(ST_EMPTY, Ordering::Release);
            e.hash.store(0, Ordering::Release);
            e.flags.store(0, Ordering::Release);
            e.key_off.store(0, Ordering::Release);
            e.key_len.store(0, Ordering::Release);
            e.val_off.store(0, Ordering::Release);
            e.val_len.store(0, Ordering::Release);
        }
        {
            let h = self.header();
            h.arena_used.store(0, Ordering::Relaxed);
            h.arena_garbage.store(0, Ordering::Relaxed);
        }

        for k in keep.iter() {
            let hash = fnv1a(&k.key);
            let (i, _) = match self.probe(&k.key, hash) {
                Some(v) => v,
                None => continue,
            };
            let e = &self.index()[i];

            let key_off = match self.arena_alloc(&k.key) {
                Some(o) => o,
                None => continue,
            };
            e.key_off.store(key_off, Ordering::Release);
            e.key_len.store(k.key.len() as u32, Ordering::Release);
            e.hash.store(hash, Ordering::Release);
            e.flags.store(k.flags, Ordering::Release);

            match &k.val {
                Some(v) => {
                    let val_off = match self.arena_alloc(v) {
                        Some(o) => o,
                        None => continue,
                    };
                    e.val_off.store(val_off, Ordering::Release);
                    e.val_len.store(v.len() as u32, Ordering::Release);
                    e.state.store(ST_LIVE, Ordering::Release);
                }
                None => {
                    e.val_off.store(0, Ordering::Release);
                    e.val_len.store(0, Ordering::Release);
                    e.state.store(ST_TOMB, Ordering::Release);
                }
            }
        }

        self.arena_end();
    }

    /// Empty the store completely. Caller must hold the semaphore.
    pub fn clear(&self) {
        self.arena_begin();

        for e in self.index().iter() {
            e.state.store(ST_EMPTY, Ordering::Release);
            e.hash.store(0, Ordering::Release);
            e.flags.store(0, Ordering::Release);
            e.key_off.store(0, Ordering::Release);
            e.key_len.store(0, Ordering::Release);
            e.val_off.store(0, Ordering::Release);
            e.val_len.store(0, Ordering::Release);
        }
        {
            let h = self.header();
            h.arena_used.store(0, Ordering::Relaxed);
            h.arena_garbage.store(0, Ordering::Relaxed);
        }

        self.arena_end();

        // Every cached value everywhere is now wrong. Normalising the parity
        // here rather than adding 2 also repairs any slot left odd by a
        // writer that died - `clear` holds the lock, so nothing is writing.
        for s in self.header().slots.iter() {
            let v = s.load(Ordering::Relaxed);
            s.store(Self::next_even(v), Ordering::Release);
        }
        self.header().global_gen.fetch_add(1, Ordering::AcqRel);
    }

    // -------------------------------------------------------- enumeration --

    /// Visit every live key/value. Caller must hold the semaphore, which is
    /// what makes the snapshot consistent.
    pub fn for_each_live<F: FnMut(&[u8], &[u8])>(&self, mut f: F) {
        for e in self.index().iter() {
            if e.state.load(Ordering::Acquire) != ST_LIVE {
                continue;
            }
            let key = match self.arena_slice(
                e.key_off.load(Ordering::Acquire),
                e.key_len.load(Ordering::Acquire),
            ) {
                Some(k) => k,
                None => continue,
            };
            let val = match self.arena_slice(
                e.val_off.load(Ordering::Acquire),
                e.val_len.load(Ordering::Acquire),
            ) {
                Some(v) => v,
                None => continue,
            };
            f(&key, &val);
        }
    }

    // ------------------------------------------------------------ commit --

    /// Collect everything the on-disk store owes. Caller must hold the
    /// semaphore.
    ///
    /// Visits every live key, not only the dirty ones: `needs_write` is asked
    /// about the clean ones, and answering yes is what lets commit
    /// re-establish the store after something has changed `/nvram` behind the
    /// library's back. "Commit makes disk match memory" is what the name
    /// promises and what Tomato's whole-buffer commit always did.
    ///
    /// `needs_write` gets the value's *length*, not its bytes, so the returned
    /// snapshot only ever copies the values actually being written. In steady
    /// state that is a handful of entries rather than the whole store.
    pub fn commit_snapshot<F: FnMut(&[u8], usize) -> bool>(
        &self,
        mut needs_write: F,
    ) -> Vec<Pending> {
        let mut out = Vec::new();

        for e in self.index().iter() {
            let state = e.state.load(Ordering::Acquire);
            if state == ST_EMPTY {
                continue;
            }
            let dirty = (e.flags.load(Ordering::Acquire) & FL_DIRTY) != 0;
            // Nothing to do for a deletion that has already been committed.
            if state == ST_TOMB && !dirty {
                continue;
            }
            let key = match self.arena_slice(
                e.key_off.load(Ordering::Acquire),
                e.key_len.load(Ordering::Acquire),
            ) {
                Some(k) => k,
                None => continue,
            };
            let hash = fnv1a(&key);
            let seen = self.slot_value(self.slot_of(hash));

            let val = if state == ST_LIVE {
                let len = e.val_len.load(Ordering::Acquire);
                if !dirty && !needs_write(&key, len as usize) {
                    continue;
                }
                match self.arena_slice(e.val_off.load(Ordering::Acquire), len) {
                    Some(v) => Some(v),
                    None => continue,
                }
            } else {
                None
            };

            out.push(Pending {
                key,
                val,
                hash,
                seen,
            });
        }

        out
    }

    /// Clear the dirty flag on every entry that has not changed since its
    /// snapshot was taken. Caller must hold the semaphore.
    ///
    /// The check is the point. The file writes run *without* the writer lock,
    /// so a key can be set again while its old value is still on its way to
    /// disk. Clearing the flag unconditionally would strand that new value in
    /// memory until something else happened to dirty the key. An unchanged
    /// seqlock counter is what says no such write landed - conservatively, in
    /// that keys sharing a counter make each other look changed, which costs a
    /// redundant write next commit and never a lost one.
    pub fn commit_writeback(&self, done: &[Pending]) {
        for p in done.iter() {
            if self.slot_value(self.slot_of(p.hash)) != p.seen {
                continue;
            }
            // Re-probe rather than remembering an index: a compaction between
            // the two phases moves entries about, and it carries the flags
            // across with them.
            if let Some((i, true)) = self.probe(&p.key, p.hash) {
                self.index()[i].flags.fetch_and(!FL_DIRTY, Ordering::AcqRel);
            }
        }
    }
}

/// One thing the disk owes, as of the snapshot that produced it.
pub struct Pending {
    pub key: Vec<u8>,
    /// `None` means the key was unset and its file needs unlinking.
    pub val: Option<Vec<u8>>,
    pub hash: u64,
    /// The key's seqlock counter when the snapshot was taken.
    pub seen: u64,
}

impl Drop for Segment {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.base as *mut libc::c_void, SEGMENT_SIZE) };
    }
}

// The segment is process-shared by construction; Tomato64 has no threaded
// consumers (redesign brief section 10.7), and all mutation is serialised by
// the SysV semaphore.
unsafe impl Send for Segment {}
unsafe impl Sync for Segment {}
