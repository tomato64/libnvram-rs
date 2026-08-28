//! The SysV semaphores: one that serialises writers, one that serialises
//! commits against each other.
//!
//! The writer lock is kept deliberately compatible with the C library's: same
//! `ftok()` key, same `SEM_UNDO` semantics. `SEM_UNDO` is load-bearing - it makes the kernel
//! release the lock if a process dies holding it, which is the whole reason
//! not to use a process-shared futex here.
//!
//! The read path does not normally take either: see
//! `shm::Segment::read_value`. It reaches for the writer lock in two places
//! only - `validate()`, which blocks on it to wait out a segment still being
//! populated, and the torn-read fallback in `Store::get`, whose whole purpose
//! is to hold it so that nothing can be writing.

use crate::consts::{COMMIT_IPC_KEY, IPC_KEY};
use crate::paths::store_root;
use std::ffi::CString;

/// Look up (creating if needed) a shared semaphore. Returns -1 on failure.
fn sem_get(proj: libc::c_int) -> libc::c_int {
    // The key is derived from the store path, so every process computes the
    // same one independently.
    let path = match CString::new(store_root()) {
        Ok(p) => p,
        Err(_) => return -1,
    };

    let key = unsafe { libc::ftok(path.as_ptr(), proj) };
    if key == -1 {
        return -1;
    }

    // Try to create it exclusively; whoever wins initialises it to 1.
    let semid = unsafe { libc::semget(key, 1, libc::IPC_CREAT | libc::IPC_EXCL | 0o666) };
    if semid >= 0 {
        // SETVAL rather than semop(+1): it is a single atomic initialisation
        // and carries no SEM_UNDO adjustment, which is what we want for a
        // value that must survive the creating process.
        //
        // semctl is variadic; SETVAL reads the first member of the classic
        // `union semun`, which is an int, so passing one directly is correct.
        if unsafe { libc::semctl(semid, 0, libc::SETVAL, 1 as libc::c_int) } == -1 {
            unsafe { libc::semctl(semid, 0, libc::IPC_RMID) };
            return -1;
        }
        return semid;
    }

    if unsafe { *libc::__errno_location() } != libc::EEXIST {
        return -1;
    }

    // Someone else created it. If they have not initialised it yet the value
    // is still 0, and our semop(-1) below simply blocks until they set it -
    // which is the correct behaviour, and needs no explicit handshake.
    //
    // The C library instead spun on IPC_STAT waiting for sem_otime to become
    // non-zero. That requires `struct semid_ds`, which the libc crate only
    // defines for glibc - its layout is arch-dependent and musl is not
    // covered. Blocking in semop achieves the same synchronisation without it.
    unsafe { libc::semget(key, 1, 0) }
}

/// RAII lock. Dropping it releases the semaphore.
pub struct Guard {
    semid: libc::c_int,
}

impl Guard {
    /// Acquire the writer lock. Returns `None` if the semaphore is
    /// unavailable, in which case the caller must fail the operation rather
    /// than proceed unlocked.
    pub fn acquire() -> Option<Guard> {
        Guard::lock(IPC_KEY)
    }

    /// Acquire the commit lock, held for the whole of `nvram_commit()`.
    ///
    /// A separate lock because commit deliberately drops the *writer* lock
    /// around its file writes, so that a first boot's thousand-odd
    /// synchronous writes no longer stall every other process's
    /// `nvram_set()`. What that opens up is two commits interleaving writes
    /// to the same key, where the one holding the older snapshot could land
    /// last; this closes it.
    ///
    /// Deliberately its own semaphore *set* rather than a second semaphore in
    /// the existing one: the C library created that set with `nsems = 1`, and
    /// `semget()` against an existing set with a larger `nsems` fails with
    /// `EINVAL`, so growing it would break a partially upgraded userland.
    /// `SEM_UNDO` applies here too - a committer that dies releases it.
    pub fn acquire_commit() -> Option<Guard> {
        Guard::lock(COMMIT_IPC_KEY)
    }

    fn lock(proj: libc::c_int) -> Option<Guard> {
        let semid = sem_get(proj);
        if semid == -1 {
            return None;
        }
        // Retry on EINTR, which is not optional here. `semop` is on the
        // never-restarted list in signal(7), so `SA_RESTART` does not cover
        // it - and musl's `signal()` sets `SA_RESTART`, so a caller that
        // installed a handler the ordinary way has no idea. This library is
        // linked into pid 1, which installs a `SIGCHLD` handler and forks
        // constantly; without this loop a child exiting while `nvram_set`
        // waits for the lock makes the set return `E_FAILURE` having stored
        // nothing, and `rc` does not check the return.
        loop {
            let mut sb = libc::sembuf {
                sem_num: 0,
                sem_op: -1,
                sem_flg: libc::SEM_UNDO as libc::c_short,
            };
            if unsafe { libc::semop(semid, &mut sb, 1) } == 0 {
                return Some(Guard { semid });
            }
            if unsafe { *libc::__errno_location() } != libc::EINTR {
                return None;
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Releasing never blocks, so EINTR is close to impossible here - but
        // dropping the lock on the floor would wedge every writer on the box,
        // so the loop is cheap insurance. `SEM_UNDO` is the real backstop.
        loop {
            let mut sb = libc::sembuf {
                sem_num: 0,
                sem_op: 1,
                sem_flg: libc::SEM_UNDO as libc::c_short,
            };
            if unsafe { libc::semop(self.semid, &mut sb, 1) } == 0 {
                return;
            }
            if unsafe { *libc::__errno_location() } != libc::EINTR {
                return;
            }
        }
    }
}

/// Test-only: destroy both semaphore sets for the current store path.
///
/// The firmware never does this - they are recreated on demand and cost two
/// entries per boot. The test suite gives every test its own scratch
/// directory, so without a sweep it orphans a pair per test and walks towards
/// `SEMMNI`.
#[cfg(feature = "testing")]
pub fn remove_semaphores() {
    let path = match CString::new(store_root()) {
        Ok(p) => p,
        Err(_) => return,
    };
    for proj in [IPC_KEY, COMMIT_IPC_KEY] {
        let key = unsafe { libc::ftok(path.as_ptr(), proj) };
        if key == -1 {
            continue;
        }
        let semid = unsafe { libc::semget(key, 1, 0) };
        if semid >= 0 {
            unsafe { libc::semctl(semid, 0, libc::IPC_RMID) };
        }
    }
}
