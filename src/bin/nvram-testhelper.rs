//! Test-only helper: performs one NVRAM operation in a separate process.
//!
//! Exists so the suite can exercise genuine cross-process behaviour - the
//! property the whole shared-segment design turns on - rather than simulating
//! it with fork(). It also mirrors the real firmware shape, where httpd
//! spawns the `nvram` CLI to do the work and then commits in the parent.
//!
//! Usage: nvram-testhelper <root> <shm> <command> [args...]

use std::ffi::CString;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: {} <root> <shm> <command> [args...]", args[0]);
        std::process::exit(2);
    }

    nvram::__set_paths(&args[1], &args[2]);

    let cmd = args[3].as_str();
    let rest = &args[4..];

    let code = match cmd {
        "set" if rest.len() == 2 => {
            let k = CString::new(rest[0].as_str()).unwrap();
            let v = CString::new(rest[1].as_str()).unwrap();
            if nvram::nvram_set(k.as_ptr(), v.as_ptr()) == 1 {
                0
            } else {
                1
            }
        }
        "unset" if rest.len() == 1 => {
            let k = CString::new(rest[0].as_str()).unwrap();
            nvram::nvram_unset(k.as_ptr());
            0
        }
        "get" if rest.len() == 1 => {
            let k = CString::new(rest[0].as_str()).unwrap();
            let p = nvram::nvram_get(k.as_ptr());
            if p.is_null() {
                println!("<absent>");
            } else {
                let s = unsafe { std::ffi::CStr::from_ptr(p) };
                println!("{}", s.to_string_lossy());
            }
            0
        }
        // Bring the shared store up from another process, so the parent can
        // exercise reattaching to a segment it did not create.
        "init" => {
            if nvram::nvram_init(std::ptr::null_mut()) == 1 {
                0
            } else {
                1
            }
        }
        "commit" => {
            if nvram::nvram_commit() == 1 {
                0
            } else {
                1
            }
        }
        // Commit, stalling between the snapshot and the writes. That is the
        // window where commit deliberately holds no writer lock, so it is the
        // window the suite needs to be able to act in.
        "commit-slow" if rest.len() == 1 => {
            nvram::__set_commit_pause_ms(rest[0].parse().unwrap_or(0));
            if nvram::nvram_commit() == 1 {
                0
            } else {
                1
            }
        }
        // Hold the writer lock for `ms`, so the parent can be made to block
        // on it and then be interrupted by a signal.
        "hold-lock" if rest.len() == 1 => {
            if nvram::__hold_writer_lock_ms(rest[0].parse().unwrap_or(0)) {
                0
            } else {
                1
            }
        }
        // Write `key` alternating between a long and a short value, `n` times.
        // Drives the seqlock and arena compaction against a live reader.
        "churn" if rest.len() == 2 => {
            let key = CString::new(rest[0].as_str()).unwrap();
            let n: usize = rest[1].parse().unwrap_or(0);
            let long = CString::new("L".repeat(40_000)).unwrap();
            let short = CString::new("S").unwrap();
            for i in 0..n {
                let v = if i % 2 == 0 { &long } else { &short };
                nvram::nvram_set(key.as_ptr(), v.as_ptr());
            }
            0
        }
        // Set `n` distinct keys, to prove concurrent writers don't lose updates.
        "setmany" if rest.len() == 2 => {
            let prefix = rest[0].as_str();
            let n: usize = rest[1].parse().unwrap_or(0);
            for i in 0..n {
                let k = CString::new(format!("{}{}", prefix, i)).unwrap();
                let v = CString::new(format!("v{}", i)).unwrap();
                nvram::nvram_set(k.as_ptr(), v.as_ptr());
            }
            0
        }
        _ => {
            eprintln!("bad command: {} ({} args)", cmd, rest.len());
            2
        }
    };

    std::process::exit(code);
}
