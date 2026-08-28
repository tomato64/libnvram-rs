//! Where the store lives.
//!
//! In a production build these are the compile-time constants and nothing can
//! change them. The `testing` feature (never enabled for the firmware) adds
//! runtime overrides so the test suite can point at a scratch directory and a
//! private shared-memory object instead of `/nvram` and the real segment.

use crate::consts::{MOUNT_POINT, SHM_NAME};

#[cfg(not(feature = "testing"))]
pub fn store_root() -> String {
    MOUNT_POINT.to_string()
}

#[cfg(not(feature = "testing"))]
pub fn shm_name() -> Vec<u8> {
    SHM_NAME.to_vec()
}

#[cfg(feature = "testing")]
mod overrides {
    use std::cell::RefCell;

    thread_local! {
        pub static ROOT: RefCell<Option<String>> = const { RefCell::new(None) };
        pub static SHM: RefCell<Option<String>> = const { RefCell::new(None) };
    }
}

#[cfg(feature = "testing")]
pub fn store_root() -> String {
    overrides::ROOT.with(|r| {
        r.borrow()
            .clone()
            .unwrap_or_else(|| MOUNT_POINT.to_string())
    })
}

#[cfg(feature = "testing")]
pub fn shm_name() -> Vec<u8> {
    overrides::SHM.with(|s| match s.borrow().as_ref() {
        Some(n) => {
            let mut v = n.as_bytes().to_vec();
            v.push(0);
            v
        }
        None => SHM_NAME.to_vec(),
    })
}

/// Test-only: point the library at a scratch store.
#[cfg(feature = "testing")]
pub fn set_paths(root: &str, shm: &str) {
    let mut root = root.to_string();
    if !root.ends_with('/') {
        root.push('/');
    }
    overrides::ROOT.with(|r| *r.borrow_mut() = Some(root));
    overrides::SHM.with(|s| *s.borrow_mut() = Some(shm.to_string()));
}
