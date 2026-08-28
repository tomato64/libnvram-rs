# libnvram-rs

`libnvram.so` for [Tomato64](https://github.com/tomato64/tomato64) — a
shared-memory NVRAM store, written in Rust. Drop-in ABI replacement for the
library it succeeds; no caller changes are required.

## Design

Three tiers:

| Tier | What | Written by |
|---|---|---|
| Shared segment | Authoritative live store: values, per-key seqlock counters, dirty flags | Any process, under the SysV semaphore |
| Per-process cache | Private copies of the keys *this* process has read | Only this process |
| `/nvram/` | Persistence, one file per key | Only `nvram_commit()` |

The shared segment is a fixed-size POSIX shared-memory object, so the mapping
never moves. Values live in a 1 MB arena behind an open-addressed key index.

## Features

**Lock-free reads.** Each key hashes to a seqlock counter, and a cached value
stays valid while that counter is unchanged — a hit is a hash lookup and one
load from an already-mapped page, with no syscall and no copy. The counter also
serves as the write-in-progress flag and torn-read detector.

**Reads do not accumulate memory.** `nvram_get()` returns a library-owned copy
that is reused on every later read of the same key, so a process's memory is
bounded by the number of distinct variables it touches rather than by how many
times it reads them. Private copies also keep pointers stable, where a pointer
into the shared arena would rot as soon as another process compacted it.

**A set is visible everywhere at once; commit is the durability barrier.** The
dirty set lives in the segment rather than per process, so any process's
`nvram_commit()` flushes every process's pending writes — which is what lets
`nvram restore` set values in a child that then exits and have the parent
commit them. Values re-derived from hardware each boot, and variables used for
message passing, never reach the flash at all.

**Atomic per-key writes, batched.** Commit stages each changed value into a
temp file, flushes the filesystem once, then renames each into place — so a
crash leaves every key holding its old or its new value, never a truncated or
empty one. Batching the flush is what makes it fast: ~3,500 keys cost one
filesystem flush instead of an `fsync` each, which on flash is the difference
between a fraction of a second and tens of seconds. Only the NVRAM filesystem
is flushed, not every mounted one.

**Commit never stalls other processes.** The file writes run holding no writer
lock — it is taken only to decide what the disk owes, and again to clear the
dirty flags afterwards. A second semaphore serialises commits against each
other.

**The store heals itself.** A clean key whose file has gone missing or changed
length is rewritten, so `/nvram` re-establishes itself if something edits it
directly. Contents are not compared, so a same-length edit is not detected.

**Bringing the store up is explicit.** `nvram_init()` reads `/nvram` into the
segment; rc calls it once, after the store is mounted and before anything reads
a setting. Until then, calls read and write the files directly, so nothing
silently depends on it. The segment records which filesystem it was built from
and refuses to serve one built from a directory since shadowed by a mount.

## Contract

* `nvram_get()` returns a pointer **owned by the library** — never free it. It
  stays valid at least until that key next changes.
* A null return means the key does not exist, distinctly from a pointer to
  `""`, which means it exists and is empty. Callers depend on this.
* `nvram_set()` is visible to every process immediately, but is **not** durable
  until `nvram_commit()`.
* A name that cannot be stored as a file — containing `/`, starting with `.`,
  or too long — is refused by `nvram_set()` and `nvram_unset()`, rather than
  accepted and then failing every later commit in every process.
* `nvram_unset()` returns `E_SUCCESS` for a key that does not exist; there is
  nothing left to do.
* `E_SUCCESS` is **1** and `E_FAILURE` is **0** — not the usual C convention.

## Building

```sh
cargo build --release        # -> target/release/libnvram.so
cargo test --features testing -- --test-threads=1
```

The `testing` feature adds runtime path overrides so the suite can use a
scratch directory instead of `/nvram`, and builds a helper binary for the
cross-process tests. It is never enabled for firmware builds.

Tests must run single-threaded: the library is single-threaded by contract and
the harness swaps process-global paths between tests.

## Assumptions

* **No consumer is threaded** (verified across cstats, rstats, rc, httpd,
  nvram, mdu, wanuptime and dhcp6c), so the per-process cache carries no lock.
* **`panic = "abort"`** — unwinding across the FFI boundary is undefined
  behaviour, and this library is linked into pid 1.
