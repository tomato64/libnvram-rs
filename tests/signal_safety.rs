//! A signal delivered while waiting for the writer lock.
//!
//! `semop(2)` is on the never-restarted list in signal(7): `SA_RESTART` does
//! not cover it, and both glibc's and musl's `signal()` set `SA_RESTART`, so
//! a caller that installed a handler the ordinary way has no reason to expect
//! `EINTR`. This library is linked into pid 1, which installs a `SIGCHLD`
//! handler and forks constantly.
//!
//! Without a retry loop, a child exiting while `nvram_set()` waits for the
//! lock made the set return `E_FAILURE` having stored nothing - and `rc`
//! does not check the return, so the setting was gone with no diagnostic.
//! That was a regression against the C library, which also ignored `EINTR`
//! but then proceeded *unlocked* and still wrote the file.

mod common;
use common::*;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

fn helper_bin() -> PathBuf {
    let mut p = std::env::current_exe().expect("current_exe");
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("nvram-testhelper")
}

fn spawn_lock_holder(s: &Scratch, ms: u64) -> std::process::Child {
    Command::new(helper_bin())
        .arg(s.root.to_str().unwrap())
        .arg(&s.shm)
        .arg("hold-lock")
        .arg(ms.to_string())
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn helper")
}

extern "C" fn noop_handler(_: libc::c_int) {}

/// Deliver `sig` to *this* thread, `after` from now.
///
/// `alarm()` is not enough: it signals the process, and the kernel hands the
/// signal to whichever thread is not blocking it - in a Rust test binary that
/// is the harness's main thread, not the thread running the test body, so the
/// `semop` under test is never interrupted and the test passes against the
/// bug. `pthread_kill` names the target.
fn signal_self_after(sig: libc::c_int, after: Duration) -> std::thread::JoinHandle<()> {
    let target = unsafe { libc::pthread_self() } as usize;
    std::thread::spawn(move || {
        std::thread::sleep(after);
        unsafe { libc::pthread_kill(target as libc::pthread_t, sig) };
    })
}

/// Install a handler the way `rc` does: `SA_RESTART` set, which is what makes
/// this failure invisible to the caller.
fn install_restarting_handler(sig: libc::c_int) {
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = noop_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
    }
}

#[test]
fn a_signal_while_waiting_for_the_lock_does_not_discard_the_write() {
    let s = Scratch::new("signalset");
    install_restarting_handler(libc::SIGALRM);

    // Someone else holds the writer lock for two seconds.
    let mut holder = spawn_lock_holder(&s, 2000);
    std::thread::sleep(Duration::from_millis(300));

    // Fire a signal one second from now - while we are blocked in semop.
    let signaller = signal_self_after(libc::SIGALRM, Duration::from_millis(1000));
    let rc = set("wan_proto", "dhcp");
    let _ = signaller.join();
    let _ = holder.wait();

    assert_eq!(
        rc, E_SUCCESS,
        "nvram_set reported failure after being interrupted by a signal"
    );
    assert_eq!(
        get("wan_proto").as_deref(),
        Some("dhcp"),
        "the value was silently discarded"
    );
}

#[test]
fn a_signal_while_waiting_does_not_discard_an_unset() {
    let s = Scratch::new("signalunset");
    install_restarting_handler(libc::SIGALRM);

    assert_eq!(set("doomed", "value"), E_SUCCESS);

    let mut holder = spawn_lock_holder(&s, 2000);
    std::thread::sleep(Duration::from_millis(300));

    let signaller = signal_self_after(libc::SIGALRM, Duration::from_millis(1000));
    let rc = unset("doomed");
    let _ = signaller.join();
    let _ = holder.wait();

    assert_eq!(rc, E_SUCCESS);
    assert_eq!(get("doomed"), None, "the unset was silently discarded");
}

#[test]
fn a_signal_while_waiting_does_not_abort_a_commit() {
    let s = Scratch::new("signalcommit");
    install_restarting_handler(libc::SIGALRM);

    assert_eq!(set("lan_ipaddr", "192.168.1.1"), E_SUCCESS);

    let mut holder = spawn_lock_holder(&s, 2000);
    std::thread::sleep(Duration::from_millis(300));

    let signaller = signal_self_after(libc::SIGALRM, Duration::from_millis(1000));
    let rc = commit();
    let _ = signaller.join();
    let _ = holder.wait();

    assert_eq!(rc, E_SUCCESS, "commit reported failure after a signal");
    assert_eq!(s.disk_read_str("lan_ipaddr").as_deref(), Some("192.168.1.1"));
}
