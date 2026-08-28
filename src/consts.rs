//! Compile-time layout and compatibility constants.
//!
//! The values in the "ABI" section are dictated by the C library this crate
//! replaces; changing them breaks every caller in the firmware. See the
//! Tomato64 NVRAM redesign brief, section 15 (compatibility checklist).

// ---------------------------------------------------------------- ABI ----

/// Return value for success. **Not** zero - the C libnvram this replaces
/// defines `E_SUCCESS` as 1, and callers test the result truthily.
pub const E_SUCCESS: libc::c_int = 1;
/// Return value for failure.
pub const E_FAILURE: libc::c_int = 0;

/// Directory holding the persistent store, one file per key.
pub const MOUNT_POINT: &str = "/nvram/";

/// Reported by `nvram_get_nvramspace()`. Also the arena size below.
pub const NVRAM_SPACE: usize = 1024 * 1024;

/// `ftok()` project id for the SysV semaphore. Must match the C library so a
/// partially upgraded userland still interlocks correctly.
pub const IPC_KEY: libc::c_int = 'A' as libc::c_int;

/// `ftok()` project id for the commit lock, which serialises `nvram_commit()`
/// against itself. Ours alone - the C library has no equivalent, because it
/// wrote through on every set and had nothing to defer.
pub const COMMIT_IPC_KEY: libc::c_int = 'B' as libc::c_int;


// ------------------------------------------------------------- layout ----

/// POSIX shared memory object name (lives under /dev/shm).
pub const SHM_NAME: &[u8] = b"/tomato64-nvram\0";

/// "NVRM"
pub const SHM_MAGIC: u32 = 0x4E56_524D;
/// Bumped whenever the on-segment layout changes incompatibly.
///
/// 2: added `Header::store_dev`, and made segment creation explicit rather
///    than a side effect of the first NVRAM call.
pub const SHM_VERSION: u32 = 2;

/// How often a process running without the segment retries attaching, in
/// operations. Reattaching is what lets a process that ran before
/// `nvram_init()` pick the segment up afterwards; throttling it keeps a
/// system where `nvram_init()` is never called from paying a failed
/// `shm_open` on every single call.
pub const REATTACH_INTERVAL: u32 = 16;

/// Number of seqlock counters. Keys map onto these by hash, so collisions are
/// harmless - they cost an unnecessary re-copy, never a wrong value. Fixed
/// size is what lets the mapping never move.
pub const SLOTS: usize = 4096;

/// Buckets in the open-addressed key index.
///
/// A real store is ~3,500 keys (measured: 3,462 files in `/nvram` on a live
/// box), which is a load factor of ~42%. Linear probing costs about two probes
/// for a miss at that load, against `MAX_PROBES` of 64 - so there is a wide
/// margin, but it is not the 20% the first sizing pass assumed. Raising this
/// changes the segment layout and so requires bumping `SHM_VERSION`.
pub const INDEX_SLOTS: usize = 8192;

/// Bytes of variable-length key/value storage.
pub const ARENA_SIZE: usize = NVRAM_SPACE;

/// Compact once garbage exceeds this fraction of the arena (as a divisor:
/// 4 means "more than a quarter is garbage").
pub const COMPACT_GARBAGE_DIVISOR: usize = 4;

/// Maximum probes before an index lookup gives up. Guards against a corrupted
/// segment turning a lookup into an infinite loop.
pub const MAX_PROBES: usize = 64;

/// Bounded retries for a seqlock read before falling back to the slow path.
pub const MAX_SEQ_RETRIES: u32 = 1000;
